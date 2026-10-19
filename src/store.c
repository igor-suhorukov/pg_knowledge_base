/* SPDX-License-Identifier: Apache-2.0 */
/*-------------------------------------------------------------------------
 *
 * store.c
 *	  The store (docs/design.md §3): kb.quads read by index-only scans of
 *	  its permutation indexes, the dictionary kb.terms, the named graphs
 *	  kb.graphs, and the writes to them -- the KbHost callbacks the engine's
 *	  dataset (engine/src/dataset.rs) and writer (engine/src/writer.rs) use.
 *
 * A quad is four uuid keys (engine/src/key.rs); the default graph's is all
 * zeros.  kb.quads has six btree indexes, oxigraph's nine in PostgreSQL's
 * terms: (g, s, p, o), (g, p, o, s) and (g, o, s, p) over every quad, and
 * (s, p, o, g), (p, o, s, g) and (o, s, p, g) over the quads of named graphs
 * alone -- partial indexes, WHERE g <> zeros -- as oxigraph keeps the
 * default graph's triples in three orders and named graphs' quads in six.
 * A scan takes the index whose leading columns the pattern binds most of,
 * and reads the quads from its tuples, visiting the heap only for a page
 * the visibility map does not show all-visible, as nodeIndexonlyscan.c's
 * IndexOnlyNext() does (PG19:src/backend/executor/nodeIndexonlyscan.c:62).
 * The indexes are found by their columns, not their names, so that an
 * administrator may drop the three partial ones (the g-first ones then
 * serve named graphs too, by btree's skip scan) or add others.
 *
 * Reads are the caller's snapshot's, permissions are checked once a call
 * (SELECT on the table read), and a table under row-level security is
 * refused: these scans bypass the executor's policies.  Writes go through
 * SPI, as INSERT ... ON CONFLICT DO NOTHING and DELETE statements over
 * arrays of keys, so that they are WAL-logged, replicated, locked, checked
 * and triggered as any write is.
 *
 *-------------------------------------------------------------------------
 */
#include "postgres.h"

#include "access/genam.h"
#include "access/relscan.h"
#include "access/stratnum.h"
#include "access/table.h"
#include "access/tableam.h"
#include "access/visibilitymap.h"
#include "catalog/namespace.h"
#include "catalog/pg_am.h"
#include "catalog/pg_class.h"
#include "catalog/pg_type.h"
#include "executor/spi.h"
#include "executor/tuptable.h"
#include "miscadmin.h"
#include "storage/bufmgr.h"
#include "storage/predicate.h"
#include "utils/acl.h"
#include "utils/array.h"
#include "utils/builtins.h"
#include "utils/fmgroids.h"
#include "utils/guc.h"
#include "utils/lsyscache.h"
#include "utils/memutils.h"
#include "utils/rel.h"
#include "utils/rls.h"
#include "utils/snapmgr.h"
#include "utils/uuid.h"

#include "pg_knowledge_base.h"

#define POS_S	0
#define POS_P	1
#define POS_O	2
#define POS_G	3

#define MAX_QUAD_INDEXES 16

/* An index of kb.quads: which quad position each of its columns holds. */
typedef struct QuadIndex
{
	Relation	rel;
	int			ncols;
	int			pos[INDEX_MAX_KEYS];
	bool		named_only;		/* partial, WHERE g <> zeros */
} QuadIndex;

struct KbStore
{
	Relation	quads;
	int			attpos[4];		/* the table's attnum of s, p, o, g */
	QuadIndex	indexes[MAX_QUAD_INDEXES];
	int			nindexes;
	Relation	terms;
	Relation	terms_pkey;
	IndexScanDesc terms_scan;
	TupleTableSlot *terms_slot;
	MemoryContext termcxt;		/* terms_get()'s strings, until its next call */
	Relation	graphs;
	Relation	graphs_pkey;
};

typedef struct QuadCursor
{
	KbCursor	base;
	QuadIndex  *index;			/* NULL: a scan of kb.graphs */
	IndexScanDesc scan;
	TupleTableSlot *slot;
	Buffer		vmbuffer;
	Snapshot	snapshot;
	bool		done;			/* the scan has ended: btree would begin it again */
	pg_uuid_t	keys[4];		/* the scan keys' values: btree reads them as it goes */
} QuadCursor;

Oid
kb_relid(const char *name)
{
	Oid			nsp = get_namespace_oid(KB_SCHEMA, true);
	Oid			relid = OidIsValid(nsp) ? get_relname_relid(name, nsp) : InvalidOid;

	if (!OidIsValid(relid))
		ereport(ERROR,
				(errcode(ERRCODE_UNDEFINED_TABLE),
				 errmsg("relation \"%s.%s\" does not exist", KB_SCHEMA, name),
				 errhint("Run CREATE EXTENSION pg_knowledge_base in this database.")));
	return relid;
}

/* A table of the store, opened for the call with its privilege checked. */
static Relation
open_table(const char *name, AclMode mode)
{
	Oid			relid = kb_relid(name);
	Relation	rel = table_open(relid, AccessShareLock);
	AclResult	aclresult = pg_class_aclcheck(relid, GetUserId(), mode);

	if (aclresult != ACLCHECK_OK)
		aclcheck_error(aclresult, OBJECT_TABLE, RelationGetRelationName(rel));
	if (check_enable_rls(relid, InvalidOid, false) == RLS_ENABLED)
		ereport(ERROR,
				(errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
				 errmsg("row-level security on %s.%s is not supported", KB_SCHEMA, name),
				 errdetail("SPARQL reads the store by index scans, past the executor's policies.")));
	return rel;
}

static KbStore *
store_of(KbCall *call)
{
	if (call->store == NULL)
		call->store = MemoryContextAllocZero(call->mcxt, sizeof(KbStore));
	return call->store;
}

static int
position_of(const char *attname)
{
	if (strcmp(attname, "s") == 0)
		return POS_S;
	if (strcmp(attname, "p") == 0)
		return POS_P;
	if (strcmp(attname, "o") == 0)
		return POS_O;
	if (strcmp(attname, "g") == 0)
		return POS_G;
	return -1;
}

/* kb.quads and its indexes, opened once a call. */
static KbStore *
open_quads(KbCall *call)
{
	KbStore    *st = store_of(call);
	TupleDesc	desc;
	List	   *indexes;
	ListCell   *lc;
	int			i;

	if (st->quads != NULL)
		return st;
	st->quads = open_table("quads", ACL_SELECT);
	desc = RelationGetDescr(st->quads);
	for (i = 0; i < 4; i++)
		st->attpos[i] = -1;
	for (i = 0; i < desc->natts; i++)
	{
		Form_pg_attribute att = TupleDescAttr(desc, i);
		int			pos;

		if (att->attisdropped)
			continue;
		pos = position_of(NameStr(att->attname));
		if (pos >= 0)
		{
			if (att->atttypid != UUIDOID)
				elog(ERROR, "column %s of %s.quads is not a uuid", NameStr(att->attname), KB_SCHEMA);
			st->attpos[pos] = att->attnum;
		}
	}
	for (i = 0; i < 4; i++)
		if (st->attpos[i] < 0)
			elog(ERROR, "%s.quads lacks one of its columns s, p, o, g", KB_SCHEMA);

	indexes = RelationGetIndexList(st->quads);
	foreach(lc, indexes)
	{
		Relation	irel;
		QuadIndex  *ix;
		bool		usable = true;
		int			k;

		if (st->nindexes >= MAX_QUAD_INDEXES)
			break;
		irel = index_open(lfirst_oid(lc), AccessShareLock);
		if (irel->rd_rel->relam != BTREE_AM_OID || !irel->rd_index->indisvalid ||
			!irel->rd_index->indisready || RelationGetIndexExpressions(irel) != NIL)
			usable = false;
		ix = &st->indexes[st->nindexes];
		ix->ncols = IndexRelationGetNumberOfKeyAttributes(irel);
		for (k = 0; usable && k < ix->ncols; k++)
		{
			AttrNumber	attnum = irel->rd_index->indkey.values[k];
			int			pos = -1;

			for (i = 0; i < 4; i++)
				if (st->attpos[i] == attnum)
					pos = i;
			if (pos < 0)
				usable = false;
			ix->pos[k] = pos;
		}
		/* an index-only scan needs all four positions among the key columns */
		if (usable && ix->ncols != 4)
			usable = false;
		if (usable && RelationGetIndexPredicate(irel) != NIL)
		{
			/*
			 * A partial index is taken for named graphs only, and only when
			 * it is one of the extension's own, whose predicate is g <> zeros.
			 */
			const char *name = RelationGetRelationName(irel);

			if (strcmp(name, "quads_spog") == 0 || strcmp(name, "quads_posg") == 0 ||
				strcmp(name, "quads_ospg") == 0)
				ix->named_only = true;
			else
				usable = false;
		}
		if (!usable)
		{
			index_close(irel, AccessShareLock);
			continue;
		}
		ix->rel = irel;
		st->nindexes++;
	}
	list_free(indexes);
	if (st->nindexes == 0)
		ereport(ERROR,
				(errcode(ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE),
				 errmsg("%s.quads has no btree index over its four columns", KB_SCHEMA)));
	return st;
}

/*
 * The index for a pattern: the most leading columns bound, a named-graphs
 * index preferred for named graphs, a g-first one otherwise.
 */
static QuadIndex *
choose_index(KbStore *st, const bool bound[4], int32 graphs)
{
	QuadIndex  *best = NULL;
	int			best_score = -1;
	int			i;

	for (i = 0; i < st->nindexes; i++)
	{
		QuadIndex  *ix = &st->indexes[i];
		int			prefix = 0;
		int			score;

		if (ix->named_only && graphs != KB_GRAPHS_NAMED)
			continue;
		while (prefix < ix->ncols && bound[ix->pos[prefix]])
			prefix++;
		score = prefix * 4;
		if (graphs == KB_GRAPHS_NAMED && ix->named_only)
			score += 2;
		/* with nothing bound, the order of the default graph's first index */
		if (prefix == 0 && ix->pos[0] == POS_G)
			score += 1;
		if (score > best_score)
		{
			best = ix;
			best_score = score;
		}
	}
	if (best == NULL)
		elog(ERROR, "no index of %s.quads serves the pattern", KB_SCHEMA);
	return best;
}

static void
quad_cursor_close(KbCursor *base)
{
	QuadCursor *c = (QuadCursor *) base;

	if (c->base.call->failed)
		return;					/* the abort releases what it holds */
	if (c->vmbuffer != InvalidBuffer)
	{
		ReleaseBuffer(c->vmbuffer);
		c->vmbuffer = InvalidBuffer;
	}
	if (c->scan)
	{
		index_endscan(c->scan);
		c->scan = NULL;
	}
	if (c->slot)
	{
		ExecDropSingleTupleTableSlot(c->slot);
		c->slot = NULL;
	}
}

typedef struct OpenArgs
{
	const KbKey *key[4];
	int32		graphs;
	KbCursor  **out;
} OpenArgs;

static void
do_quads_open(KbCall *call, void *arg)
{
	OpenArgs   *a = (OpenArgs *) arg;
	KbStore    *st;
	bool		bound[4];
	QuadIndex  *ix;
	QuadCursor *c;
	ScanKeyData keys[INDEX_MAX_KEYS];
	int			nkeys = 0;
	int			k;
	MemoryContext old;

	kb_call_advance(call);
	st = open_quads(call);
	KB_TRACE_COUNT(call, KB_TRACE_SCANS, 1);
	for (k = 0; k < 4; k++)
		bound[k] = a->key[k] != NULL;
	if (a->graphs == KB_GRAPHS_ONE && a->key[POS_G] == NULL)
		elog(ERROR, "a scan of one graph names no graph");
	if (a->graphs != KB_GRAPHS_ONE)
		bound[POS_G] = false;
	ix = choose_index(st, bound, a->graphs);

	old = MemoryContextSwitchTo(call->mcxt);
	c = palloc0(sizeof(QuadCursor));
	c->base.close = quad_cursor_close;
	c->index = ix;
	c->vmbuffer = InvalidBuffer;
	c->snapshot = GetActiveSnapshot();
	for (k = 0; k < ix->ncols; k++)
	{
		int			pos = ix->pos[k];

		if (bound[pos])
		{
			memcpy(c->keys[pos].data, a->key[pos]->b, UUID_LEN);
			ScanKeyInit(&keys[nkeys++], k + 1, BTEqualStrategyNumber, F_UUID_EQ,
						UUIDPGetDatum(&c->keys[pos]));
		}
		else if (pos == POS_G && a->graphs == KB_GRAPHS_NAMED && !ix->named_only)
		{
			static const pg_uuid_t zeros = {{0}};

			ScanKeyInit(&keys[nkeys++], k + 1, BTGreaterStrategyNumber, F_UUID_GT,
						UUIDPGetDatum(&zeros));
		}
	}
	c->slot = table_slot_create(st->quads, NULL);
	c->scan = index_beginscan(st->quads, ix->rel, c->snapshot, NULL, nkeys, 0,
							  call->writes ? SO_NONE : SO_HINT_REL_READ_ONLY);
	c->scan->xs_want_itup = true;
	index_rescan(c->scan, keys, nkeys, NULL, 0);
	MemoryContextSwitchTo(old);
	kb_call_register(call, &c->base);
	*a->out = &c->base;
}

static int32
host_quads_open(void *ctx, const KbKey *s, const KbKey *p, const KbKey *o, const KbKey *g,
				int32 graphs, KbCursor **cursor)
{
	OpenArgs	a;

	a.key[POS_S] = s;
	a.key[POS_P] = p;
	a.key[POS_O] = o;
	a.key[POS_G] = g;
	a.graphs = graphs;
	a.out = cursor;
	*cursor = NULL;
	return kb_call_guard((KbCall *) ctx, do_quads_open, &a);
}

typedef struct NextArgs
{
	QuadCursor *c;
	KbKey	   *out;
	int32		max;
	int32	   *n;
} NextArgs;

static void
do_quads_next(KbCall *call, void *arg)
{
	NextArgs   *a = (NextArgs *) arg;
	QuadCursor *c = a->c;
	ItemPointer tid;
	int32		n = 0;

	(void) call;
	*a->n = 0;
	if (c->base.closed || c->scan == NULL || c->done)
		return;
	while (n < a->max)
	{
		bool		from_heap = false;
		Datum		values[INDEX_MAX_KEYS];
		bool		isnull[INDEX_MAX_KEYS];
		KbKey	   *quad = &a->out[n * 4];
		int			k;

		tid = index_getnext_tid(c->scan, ForwardScanDirection);
		if (tid == NULL)
		{
			c->done = true;
			break;
		}
		if ((n & 255) == 255)
			CHECK_FOR_INTERRUPTS();
		if (!VM_ALL_VISIBLE(c->scan->heapRelation, ItemPointerGetBlockNumber(tid), &c->vmbuffer))
		{
			if (!index_fetch_heap(c->scan, c->slot))
				continue;		/* no visible version */
			ExecClearTuple(c->slot);
			if (c->scan->xs_heap_continue)
				elog(ERROR, "non-MVCC snapshots are not supported by the store's scans");
			from_heap = true;
		}
		if (c->index == NULL)
		{
			/* kb.graphs: its key alone, as the quad's graph */
			index_deform_tuple(c->scan->xs_itup, c->scan->xs_itupdesc, values, isnull);
			memset(quad, 0, sizeof(KbKey) * 4);
			memcpy(quad[POS_G].b, DatumGetUUIDP(values[0])->data, 16);
		}
		else
		{
			index_deform_tuple(c->scan->xs_itup, c->scan->xs_itupdesc, values, isnull);
			for (k = 0; k < 4; k++)
				memcpy(quad[c->index->pos[k]].b, DatumGetUUIDP(values[k])->data, 16);
		}
		if (!from_heap)
			PredicateLockPage(c->scan->heapRelation, ItemPointerGetBlockNumber(tid), c->snapshot);
		n++;
	}
	*a->n = n;
	KB_TRACE_COUNT(c->base.call, KB_TRACE_QUADS_READ, n);
}

static int32
host_quads_next(KbCursor *cursor, KbKey *quads, int32 max, int32 *n)
{
	NextArgs	a;

	a.c = (QuadCursor *) cursor;
	a.out = quads;
	a.max = max;
	a.n = n;
	*n = 0;
	return kb_call_guard(cursor->call, do_quads_next, &a);
}

static void
do_cursor_close(KbCall *call, void *arg)
{
	KbCursor   *cursor = (KbCursor *) arg;

	(void) call;
	if (!cursor->closed)
	{
		cursor->closed = true;
		cursor->close(cursor);
	}
}

static void
host_cursor_close(KbCursor *cursor)
{
	if (cursor == NULL || cursor->closed || cursor->call->failed)
		return;
	(void) kb_call_guard(cursor->call, do_cursor_close, cursor);
}

/* ---------------------------------------------------------------------
 * The named graphs
 * ---------------------------------------------------------------------
 */

static KbStore *
open_graphs(KbCall *call)
{
	KbStore    *st = store_of(call);

	if (st->graphs == NULL)
	{
		st->graphs = open_table("graphs", ACL_SELECT);
		st->graphs_pkey = index_open(RelationGetPrimaryKeyIndex(st->graphs, false), AccessShareLock);
	}
	return st;
}

typedef struct GraphsArgs
{
	KbCursor  **out;
} GraphsArgs;

static void
do_graphs_open(KbCall *call, void *arg)
{
	GraphsArgs *a = (GraphsArgs *) arg;
	KbStore    *st;
	QuadCursor *c;
	MemoryContext old;

	kb_call_advance(call);
	st = open_graphs(call);
	old = MemoryContextSwitchTo(call->mcxt);
	c = palloc0(sizeof(QuadCursor));
	c->base.close = quad_cursor_close;
	c->index = NULL;
	c->vmbuffer = InvalidBuffer;
	c->snapshot = GetActiveSnapshot();
	c->slot = table_slot_create(st->graphs, NULL);
	c->scan = index_beginscan(st->graphs, st->graphs_pkey, c->snapshot, NULL, 0, 0,
							  SO_HINT_REL_READ_ONLY);
	c->scan->xs_want_itup = true;
	index_rescan(c->scan, NULL, 0, NULL, 0);
	MemoryContextSwitchTo(old);
	kb_call_register(call, &c->base);
	*a->out = &c->base;
}

static int32
host_graphs_open(void *ctx, KbCursor **cursor)
{
	GraphsArgs	a = {cursor};

	*cursor = NULL;
	return kb_call_guard((KbCall *) ctx, do_graphs_open, &a);
}

/* Whether an index of a store's table holds a visible row with a uuid key. */
static bool
key_exists(Relation heap, Relation index, const KbKey *key)
{
	ScanKeyData skey;
	IndexScanDesc scan;
	TupleTableSlot *slot;
	bool		found;

	ScanKeyInit(&skey, 1, BTEqualStrategyNumber, F_UUID_EQ,
				UUIDPGetDatum((const pg_uuid_t *) key->b));
	slot = table_slot_create(heap, NULL);
	scan = index_beginscan(heap, index, GetActiveSnapshot(), NULL, 1, 0, SO_HINT_REL_READ_ONLY);
	index_rescan(scan, &skey, 1, NULL, 0);
	found = index_getnext_slot(scan, ForwardScanDirection, slot);
	index_endscan(scan);
	ExecDropSingleTupleTableSlot(slot);
	return found;
}

typedef struct ExistsArgs
{
	const KbKey *g;
	bool	   *exists;
} ExistsArgs;

static void
do_graph_exists(KbCall *call, void *arg)
{
	ExistsArgs *a = (ExistsArgs *) arg;
	KbStore    *st;

	kb_call_advance(call);
	st = open_graphs(call);
	*a->exists = key_exists(st->graphs, st->graphs_pkey, a->g);
}

static int32
host_graph_exists(void *ctx, const KbKey *g, bool *exists)
{
	ExistsArgs	a = {g, exists};

	*exists = false;
	return kb_call_guard((KbCall *) ctx, do_graph_exists, &a);
}

/* ---------------------------------------------------------------------
 * The dictionary
 * ---------------------------------------------------------------------
 */

typedef struct TermsArgs
{
	const KbKey *keys;
	int32		n;
	KbDictEntry *entries;
} TermsArgs;

static void
do_terms_get(KbCall *call, void *arg)
{
	TermsArgs  *a = (TermsArgs *) arg;
	KbStore    *st = store_of(call);
	MemoryContext old;
	int32		i;

	kb_call_advance(call);
	KB_TRACE_COUNT(call, KB_TRACE_TERMS_LOOKED_UP, a->n);
	if (st->terms == NULL)
	{
		st->terms = open_table("terms", ACL_SELECT);
		st->terms_pkey = index_open(RelationGetPrimaryKeyIndex(st->terms, false), AccessShareLock);
		st->termcxt = AllocSetContextCreate(call->mcxt, "pg_knowledge_base terms",
											ALLOCSET_DEFAULT_SIZES);
		old = MemoryContextSwitchTo(call->mcxt);
		st->terms_slot = table_slot_create(st->terms, NULL);
		st->terms_scan = index_beginscan(st->terms, st->terms_pkey, GetActiveSnapshot(), NULL,
										 1, 0, SO_HINT_REL_READ_ONLY);
		MemoryContextSwitchTo(old);
	}
	MemoryContextReset(st->termcxt);
	old = MemoryContextSwitchTo(st->termcxt);
	for (i = 0; i < a->n; i++)
	{
		ScanKeyData skey;
		KbDictEntry *e = &a->entries[i];

		e->found = false;
		e->value.ptr = e->extra.ptr = NULL;
		e->value.len = e->extra.len = 0;
		ScanKeyInit(&skey, 1, BTEqualStrategyNumber, F_UUID_EQ,
					UUIDPGetDatum((const pg_uuid_t *) a->keys[i].b));
		/* the snapshot may have advanced since the scan began */
		st->terms_scan->xs_snapshot = GetActiveSnapshot();
		index_rescan(st->terms_scan, &skey, 1, NULL, 0);
		if (index_getnext_slot(st->terms_scan, ForwardScanDirection, st->terms_slot))
		{
			bool		isnull;
			Datum		value = slot_getattr(st->terms_slot, 2, &isnull);
			char	   *s;

			if (!isnull)
			{
				s = TextDatumGetCString(value);
				e->value = kb_str(s);
				value = slot_getattr(st->terms_slot, 3, &isnull);
				s = isnull ? "" : TextDatumGetCString(value);
				e->extra = kb_str(s);
				e->found = true;
			}
			ExecClearTuple(st->terms_slot);
		}
		if ((i & 255) == 255)
			CHECK_FOR_INTERRUPTS();
	}
	MemoryContextSwitchTo(old);
}

static int32
host_terms_get(void *ctx, const KbKey *keys, int32 n, KbDictEntry *entries)
{
	TermsArgs	a = {keys, n, entries};

	return kb_call_guard((KbCall *) ctx, do_terms_get, &a);
}

/* ---------------------------------------------------------------------
 * Writes, through SPI
 * ---------------------------------------------------------------------
 */

typedef enum WritePlan
{
	PLAN_QUADS_INSERT,
	PLAN_QUADS_DELETE,
	PLAN_TERMS_INSERT,
	PLAN_GRAPHS_INSERT,
	PLAN_GRAPHS_DELETE,
	PLAN_CLEAR_GRAPH,
	PLAN_CLEAR_NAMED,
	PLAN_CLEAR_ALL,
	PLAN_DROP_NAMED,
	NUM_PLANS
} WritePlan;

static const char *const plan_sql[NUM_PLANS] = {
	"INSERT INTO " KB_SCHEMA ".quads (g, s, p, o) "
	"SELECT * FROM unnest($1::uuid[], $2::uuid[], $3::uuid[], $4::uuid[]) "
	"ON CONFLICT DO NOTHING",
	"DELETE FROM " KB_SCHEMA ".quads q "
	"USING unnest($1::uuid[], $2::uuid[], $3::uuid[], $4::uuid[]) AS d (g, s, p, o) "
	"WHERE q.g = d.g AND q.s = d.s AND q.p = d.p AND q.o = d.o",
	"INSERT INTO " KB_SCHEMA ".terms (key, value, extra) "
	"SELECT * FROM unnest($1::uuid[], $2::text[], $3::text[]) "
	"ON CONFLICT DO NOTHING",
	"INSERT INTO " KB_SCHEMA ".graphs (key) SELECT unnest($1::uuid[]) ON CONFLICT DO NOTHING",
	"DELETE FROM " KB_SCHEMA ".graphs WHERE key = ANY ($1::uuid[])",
	"DELETE FROM " KB_SCHEMA ".quads WHERE g = $1::uuid",
	"DELETE FROM " KB_SCHEMA ".quads WHERE g <> '00000000-0000-0000-0000-000000000000'::uuid",
	"DELETE FROM " KB_SCHEMA ".quads",
	"DELETE FROM " KB_SCHEMA ".graphs",
};

/* Each statement's parameters: uuid[] (U), text[] (T), uuid (u) */
static const char *const plan_args[NUM_PLANS] = {
	"UUUU", "UUUU", "UTT", "U", "U", "u", "", "", "",
};

static SPIPlanPtr plans[NUM_PLANS];

static SPIPlanPtr
write_plan(WritePlan which)
{
	if (plans[which] == NULL)
	{
		const char *args = plan_args[which];
		int			nargs = (int) strlen(args);
		Oid			types[4];
		SPIPlanPtr	plan;
		int			i;

		for (i = 0; i < nargs; i++)
			types[i] = args[i] == 'U' ? UUIDARRAYOID : args[i] == 'T' ? TEXTARRAYOID : UUIDOID;
		plan = SPI_prepare(plan_sql[which], nargs, types);

		if (plan == NULL)
			elog(ERROR, "SPI_prepare failed for \"%s\": %s", plan_sql[which],
				 SPI_result_code_string(SPI_result));
		if (SPI_keepplan(plan) != 0)
			elog(ERROR, "SPI_keepplan failed");
		plans[which] = plan;
	}
	return plans[which];
}

static void
run_plan(KbCall *call, WritePlan which, int nargs, Datum *values)
{
	char		nulls[8];
	int			rc;
	int			i;

	for (i = 0; i < nargs; i++)
		nulls[i] = ' ';
	if (SPI_connect() != SPI_OK_CONNECT)
		elog(ERROR, "SPI_connect failed");
	rc = SPI_execute_plan(write_plan(which), values, nulls, false, 0);
	if (rc < 0)
		elog(ERROR, "%s failed: %s", plan_sql[which], SPI_result_code_string(rc));
	SPI_finish();
	call->dirty = true;
}

/* n uuid values at a stride, as a uuid[] */
static Datum
uuid_array(const KbKey *keys, int32 n, int stride, int offset)
{
	Datum	   *elems = palloc(sizeof(Datum) * Max(n, 1));
	int32		i;

	for (i = 0; i < n; i++)
		elems[i] = UUIDPGetDatum((const pg_uuid_t *) keys[i * stride + offset].b);
	return PointerGetDatum(construct_array(elems, n, UUIDOID, UUID_LEN, false, TYPALIGN_CHAR));
}

static Datum
text_array(const KbDictEntry *entries, int32 n, bool extra)
{
	Datum	   *elems = palloc(sizeof(Datum) * Max(n, 1));
	int32		i;

	for (i = 0; i < n; i++)
	{
		const KbStr *s = extra ? &entries[i].extra : &entries[i].value;

		elems[i] = PointerGetDatum(cstring_to_text_with_len(s->ptr ? s->ptr : "", (int) s->len));
	}
	return PointerGetDatum(construct_array_builtin(elems, n, TEXTOID));
}

/* ---------------------------------------------------------------------
 * Bulk loads
 * ---------------------------------------------------------------------
 *
 * kb.load(..., bulk => true) and kb.load_file(..., bulk => true)
 * (docs/design.md §2.6): kb.quads' indexes dropped, the document's quads
 * written to a temporary table without an index, then put into kb.quads
 * once each -- those it holds already left out -- by one INSERT, and the
 * indexes built again from their definitions, as CREATE INDEX builds one:
 * sorted, its leaves filled to their fillfactor, by parallel workers when
 * the table is large.  All of it is the caller's transaction's: an error,
 * or a rollback, leaves the indexes and the quads as they were.  kb.quads
 * stays locked -- ACCESS EXCLUSIVE, as DROP INDEX locks it -- until the
 * transaction ends, so queries of other sessions wait for it.
 */
#define BULK_TABLE "kb_bulk_load_quads"

struct KbBulk
{
	List	   *indexdefs;		/* CREATE INDEX statements, unique ones first */
};

static void
spi_exec(const char *sql, int expected)
{
	int			rc = SPI_execute(sql, false, 0);

	if (rc != expected)
		elog(ERROR, "%s failed: %s", sql, SPI_result_code_string(rc));
}

KbBulk *
kb_bulk_begin(void)
{
	Oid			quads = kb_relid("quads");
	KbBulk	   *bulk = palloc0(sizeof(KbBulk));
	MemoryContext caller = CurrentMemoryContext;
	List	   *names = NIL;
	ListCell   *lc;
	uint64		i;

	if (!object_ownercheck(RelationRelationId, quads, GetUserId()))
		ereport(ERROR,
				(errcode(ERRCODE_INSUFFICIENT_PRIVILEGE),
				 errmsg("a bulk load rebuilds the indexes of kb.quads, which only its owner may do"),
				 errhint("Load without bulk.")));
	if (SPI_connect() != SPI_OK_CONNECT)
		elog(ERROR, "SPI_connect failed");
	spi_exec("LOCK TABLE " KB_SCHEMA ".quads IN ACCESS EXCLUSIVE MODE", SPI_OK_UTILITY);
	/* an index a constraint holds stays, kept up as the quads go in */
	spi_exec("SELECT i.indexrelid::regclass::text, pg_get_indexdef(i.indexrelid) "
			 "FROM pg_index i WHERE i.indrelid = '" KB_SCHEMA ".quads'::regclass "
			 "AND NOT EXISTS (SELECT 1 FROM pg_constraint c WHERE c.conindid = i.indexrelid) "
			 "ORDER BY i.indisunique DESC, i.indexrelid",
			 SPI_OK_SELECT);
	for (i = 0; i < SPI_processed; i++)
	{
		MemoryContext old = MemoryContextSwitchTo(caller);

		names = lappend(names, SPI_getvalue(SPI_tuptable->vals[i], SPI_tuptable->tupdesc, 1));
		bulk->indexdefs = lappend(bulk->indexdefs,
								  SPI_getvalue(SPI_tuptable->vals[i], SPI_tuptable->tupdesc, 2));
		MemoryContextSwitchTo(old);
	}
	foreach(lc, names)
		spi_exec(psprintf("DROP INDEX %s", (char *) lfirst(lc)), SPI_OK_UTILITY);
	spi_exec("CREATE TEMP TABLE IF NOT EXISTS " BULK_TABLE " (g uuid, s uuid, p uuid, o uuid)",
			 SPI_OK_UTILITY);
	spi_exec("TRUNCATE pg_temp." BULK_TABLE, SPI_OK_UTILITY);
	SPI_finish();
	return bulk;
}

/* A batch of a bulk load's quads, into its staging table */
static void
bulk_stage(KbCall *call, int32 op, Datum *values)
{
	Oid			types[4] = {UUIDARRAYOID, UUIDARRAYOID, UUIDARRAYOID, UUIDARRAYOID};
	int			rc;

	if (op != KB_WRITE_INSERT)
		elog(ERROR, "a bulk load deletes nothing");
	if (SPI_connect() != SPI_OK_CONNECT)
		elog(ERROR, "SPI_connect failed");
	rc = SPI_execute_with_args("INSERT INTO pg_temp." BULK_TABLE " (g, s, p, o) "
							   "SELECT * FROM unnest($1, $2, $3, $4)",
							   4, types, values, NULL, false, 0);
	if (rc != SPI_OK_INSERT)
		elog(ERROR, "a bulk load's staging failed: %s", SPI_result_code_string(rc));
	SPI_finish();
	call->dirty = true;
}

static void
bulk_context(void *arg)
{
	(void) arg;
	errcontext("putting a bulk load's quads into kb.quads and building its indexes");
}

void
kb_bulk_finish(KbBulk *bulk)
{
	ErrorContextCallback errcb;
	ListCell   *lc;
	int			nestlevel;

	errcb.callback = bulk_context;
	errcb.arg = NULL;
	errcb.previous = error_context_stack;
	error_context_stack = &errcb;
	if (SPI_connect() != SPI_OK_CONNECT)
		elog(ERROR, "SPI_connect failed");
	/* DISTINCT's sort or hash takes a maintenance operation's memory */
	nestlevel = NewGUCNestLevel();
	(void) set_config_option("work_mem", psprintf("%dkB", maintenance_work_mem),
							 PGC_USERSET, PGC_S_SESSION, GUC_ACTION_SAVE, true, 0, false);
	spi_exec("INSERT INTO " KB_SCHEMA ".quads (g, s, p, o) "
			 "SELECT DISTINCT b.g, b.s, b.p, b.o FROM pg_temp." BULK_TABLE " b "
			 "WHERE NOT EXISTS (SELECT 1 FROM " KB_SCHEMA ".quads q "
			 "WHERE q.g = b.g AND q.s = b.s AND q.p = b.p AND q.o = b.o)",
			 SPI_OK_INSERT);
	AtEOXact_GUC(true, nestlevel);
	foreach(lc, bulk->indexdefs)
		spi_exec((const char *) lfirst(lc), SPI_OK_UTILITY);
	spi_exec("DROP TABLE pg_temp." BULK_TABLE, SPI_OK_UTILITY);
	SPI_finish();
	error_context_stack = errcb.previous;
}

typedef struct QuadsWriteArgs
{
	int32		op;
	const KbKey *quads;
	int32		n;
} QuadsWriteArgs;

static void
do_quads_write(KbCall *call, void *arg)
{
	QuadsWriteArgs *a = (QuadsWriteArgs *) arg;
	MemoryContext old = MemoryContextSwitchTo(call->mcxt);
	Datum		values[4];

	if (!call->writes)
		elog(ERROR, "a write in a call that reads only");
	/* the keys come s, p, o, g; the statements take g, s, p, o */
	values[0] = uuid_array(a->quads, a->n, 4, POS_G);
	values[1] = uuid_array(a->quads, a->n, 4, POS_S);
	values[2] = uuid_array(a->quads, a->n, 4, POS_P);
	values[3] = uuid_array(a->quads, a->n, 4, POS_O);
	if (call->bulk)
		bulk_stage(call, a->op, values);
	else
		run_plan(call, a->op == KB_WRITE_DELETE ? PLAN_QUADS_DELETE : PLAN_QUADS_INSERT, 4, values);
	KB_TRACE_COUNT(call, a->op == KB_WRITE_DELETE ? KB_TRACE_QUADS_DELETED : KB_TRACE_QUADS_WRITTEN, a->n);
	MemoryContextSwitchTo(old);
}

static int32
host_quads_write(void *ctx, int32 op, const KbKey *quads, int32 n)
{
	QuadsWriteArgs a = {op, quads, n};

	if (n <= 0)
		return KB_HOST_OK;
	return kb_call_guard((KbCall *) ctx, do_quads_write, &a);
}

typedef struct TermsWriteArgs
{
	const KbKey *keys;
	const KbDictEntry *entries;
	int32		n;
} TermsWriteArgs;

static void
do_terms_write(KbCall *call, void *arg)
{
	TermsWriteArgs *a = (TermsWriteArgs *) arg;
	MemoryContext old = MemoryContextSwitchTo(call->mcxt);
	Datum		values[3];

	if (!call->writes)
		elog(ERROR, "a write in a call that reads only");
	values[0] = uuid_array(a->keys, a->n, 1, 0);
	values[1] = text_array(a->entries, a->n, false);
	values[2] = text_array(a->entries, a->n, true);
	run_plan(call, PLAN_TERMS_INSERT, 3, values);
	KB_TRACE_COUNT(call, KB_TRACE_TERMS_WRITTEN, a->n);
	MemoryContextSwitchTo(old);
}

static int32
host_terms_write(void *ctx, const KbKey *keys, const KbDictEntry *entries, int32 n)
{
	TermsWriteArgs a = {keys, entries, n};

	if (n <= 0)
		return KB_HOST_OK;
	return kb_call_guard((KbCall *) ctx, do_terms_write, &a);
}

static void
do_graphs_write(KbCall *call, void *arg)
{
	QuadsWriteArgs *a = (QuadsWriteArgs *) arg;
	MemoryContext old = MemoryContextSwitchTo(call->mcxt);
	Datum		values[1];

	if (!call->writes)
		elog(ERROR, "a write in a call that reads only");
	values[0] = uuid_array(a->quads, a->n, 1, 0);
	run_plan(call, a->op == KB_WRITE_DELETE ? PLAN_GRAPHS_DELETE : PLAN_GRAPHS_INSERT, 1, values);
	MemoryContextSwitchTo(old);
}

static int32
host_graphs_write(void *ctx, int32 op, const KbKey *keys, int32 n)
{
	QuadsWriteArgs a = {op, keys, n};

	if (n <= 0)
		return KB_HOST_OK;
	return kb_call_guard((KbCall *) ctx, do_graphs_write, &a);
}

typedef struct ClearArgs
{
	int32		what;
	const KbKey *g;
} ClearArgs;

static void
do_graph_clear(KbCall *call, void *arg)
{
	ClearArgs  *a = (ClearArgs *) arg;
	Datum		value;

	if (!call->writes)
		elog(ERROR, "a write in a call that reads only");
	switch (a->what)
	{
		case KB_CLEAR_GRAPH:
			value = UUIDPGetDatum((const pg_uuid_t *) a->g->b);
			run_plan(call, PLAN_CLEAR_GRAPH, 1, &value);
			break;
		case KB_CLEAR_DROP:
			value = UUIDPGetDatum((const pg_uuid_t *) a->g->b);
			run_plan(call, PLAN_CLEAR_GRAPH, 1, &value);
			value = uuid_array(a->g, 1, 1, 0);
			run_plan(call, PLAN_GRAPHS_DELETE, 1, &value);
			break;
		case KB_CLEAR_NAMED:
			run_plan(call, PLAN_CLEAR_NAMED, 0, NULL);
			break;
		case KB_CLEAR_DROP_NAMED:
			run_plan(call, PLAN_CLEAR_NAMED, 0, NULL);
			run_plan(call, PLAN_DROP_NAMED, 0, NULL);
			break;
		case KB_CLEAR_ALL:
			run_plan(call, PLAN_CLEAR_ALL, 0, NULL);
			run_plan(call, PLAN_DROP_NAMED, 0, NULL);
			break;
		default:
			elog(ERROR, "unknown clear %d", a->what);
	}
}

static int32
host_graph_clear(void *ctx, int32 what, const KbKey *g)
{
	ClearArgs	a = {what, g};

	if ((what == KB_CLEAR_GRAPH || what == KB_CLEAR_DROP) && g == NULL)
		return KB_HOST_ERROR;
	return kb_call_guard((KbCall *) ctx, do_graph_clear, &a);
}

/* ---------------------------------------------------------------------
 * The host's store, and its end
 * ---------------------------------------------------------------------
 */

void
kb_store_host(KbHost *host)
{
	host->quads_open = host_quads_open;
	host->quads_next = host_quads_next;
	host->quads_close = host_cursor_close;
	host->terms_get = host_terms_get;
	host->graphs_open = host_graphs_open;
	host->graph_exists = host_graph_exists;
	host->quads_write = host_quads_write;
	host->terms_write = host_terms_write;
	host->graphs_write = host_graphs_write;
	host->graph_clear = host_graph_clear;
}

/* The relations a successful call opened, closed; their locks kept. */
void
kb_store_close(KbCall *call)
{
	KbStore    *st = call->store;
	int			i;

	if (st == NULL)
		return;
	if (st->terms_scan)
		index_endscan(st->terms_scan);
	if (st->terms_slot)
		ExecDropSingleTupleTableSlot(st->terms_slot);
	if (st->terms_pkey)
		index_close(st->terms_pkey, NoLock);
	if (st->terms)
		table_close(st->terms, NoLock);
	if (st->graphs_pkey)
		index_close(st->graphs_pkey, NoLock);
	if (st->graphs)
		table_close(st->graphs, NoLock);
	for (i = 0; i < st->nindexes; i++)
		index_close(st->indexes[i].rel, NoLock);
	if (st->quads)
		table_close(st->quads, NoLock);
	call->store = NULL;
}
