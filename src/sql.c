/* SPDX-License-Identifier: Apache-2.0 */
/*-------------------------------------------------------------------------
 *
 * sql.c
 *	  SQL for R2RML (docs/design.md §4.3): the result columns of a logical
 *	  table, its rows read through SPI's cursors, each value as the lexical
 *	  form of its natural RDF literal (R2RML §10.2), and the enabled
 *	  mappings of kb.mappings -- the KbHost callbacks of engine/src/r2rml.
 *
 * The engine builds the SQL: a logical table's query, with the conditions
 * a pattern's bound terms give.  It runs here as the calling role, read
 * only, in the caller's snapshot: the tables' privileges and row-level
 * security apply as to any query of theirs, and a mapping's rr:sqlQuery
 * cannot write.  Plans are kept per backend, by their text, so a pattern
 * probed once a binding (a nested loop of the evaluator's) is planned once.
 *
 * A value's text does not depend on the session's settings -- DateStyle,
 * TimeZone, IntervalStyle, bytea_output, extra_float_digits -- so that a
 * row makes the same IRIs and literals for every session: dates and
 * timestamps in XML Schema's form (USE_XSD_DATES), timestamps with time
 * zone in UTC, intervals in ISO 8601, floating point numbers in their
 * shortest exact form, bytea in upper-case hexadecimal.
 *
 *-------------------------------------------------------------------------
 */
#include "postgres.h"

#include <math.h>

#include "catalog/pg_type.h"
#include "common/hashfn.h"
#include "common/shortest_dec.h"
#include "executor/spi.h"
#include "funcapi.h"
#include "miscadmin.h"
#include "tcop/utility.h"
#include "utils/builtins.h"
#include "utils/date.h"
#include "utils/datetime.h"
#include "utils/hsearch.h"
#include "utils/lsyscache.h"
#include "utils/memutils.h"
#include "utils/plancache.h"
#include "utils/portal.h"
#include "utils/timestamp.h"

#include "pg_knowledge_base.h"

struct KbSql
{
	KbColumn   *columns;		/* sql_describe()'s, until its next call */
	int			ncolumns;
	KbMappingRow *mappings;
	int			nmappings;
};

typedef struct SqlCursor
{
	KbCursor	base;
	Portal		portal;
	int			ncolumns;
	Oid		   *types;			/* the base types */
	int		   *classes;
	FmgrInfo   *out;
	MemoryContext rowcxt;		/* a batch's values, until the next */
	KbSqlValue *values;
} SqlCursor;

/* Plans by their text, for the backend's life */
typedef struct PlanEntry
{
	char		key[NAMEDATALEN];	/* a hash of the text, as hex */
	char	   *sql;
	SPIPlanPtr	plan;
} PlanEntry;

static HTAB *plan_cache = NULL;

#define MAX_CACHED_PLANS 2048

static void
plan_key(const char *sql, char *key)
{
	uint64		h = hash_bytes_extended((const unsigned char *) sql, (int) strlen(sql), 0);

	snprintf(key, NAMEDATALEN, "%016llx%08x", (unsigned long long) h, (unsigned) strlen(sql));
}

static void
drop_plans(void)
{
	HASH_SEQ_STATUS status;
	PlanEntry  *e;

	if (plan_cache == NULL)
		return;
	hash_seq_init(&status, plan_cache);
	while ((e = hash_seq_search(&status)) != NULL)
	{
		SPI_freeplan(e->plan);
		pfree(e->sql);
	}
	hash_destroy(plan_cache);
	plan_cache = NULL;
}

/* A plan of a SELECT with text parameters, cached; inside SPI_connect(). */
static SPIPlanPtr
cached_plan(const char *sql, int nparams)
{
	char		key[NAMEDATALEN];
	PlanEntry  *e;
	bool		found;
	Oid		   *types;
	SPIPlanPtr	plan;
	int			i;

	if (plan_cache == NULL)
	{
		HASHCTL		ctl;

		ctl.keysize = NAMEDATALEN;
		ctl.entrysize = sizeof(PlanEntry);
		plan_cache = hash_create("pg_knowledge_base plans", 256, &ctl, HASH_ELEM | HASH_STRINGS);
	}
	plan_key(sql, key);
	e = hash_search(plan_cache, key, HASH_FIND, NULL);
	if (e != NULL && strcmp(e->sql, sql) == 0)
		return e->plan;
	if (hash_get_num_entries(plan_cache) >= MAX_CACHED_PLANS)
		drop_plans();

	types = palloc(sizeof(Oid) * Max(nparams, 1));
	for (i = 0; i < nparams; i++)
		types[i] = TEXTOID;
	plan = SPI_prepare(sql, nparams, types);
	if (plan == NULL)
		ereport(ERROR,
				(errcode(ERRCODE_INVALID_OBJECT_DEFINITION),
				 errmsg("a logical table's SQL does not prepare: %s",
						SPI_result_code_string(SPI_result)),
				 errdetail("SQL: %s", sql)));
	if (!SPI_is_cursor_plan(plan))
		ereport(ERROR,
				(errcode(ERRCODE_INVALID_OBJECT_DEFINITION),
				 errmsg("a logical table's SQL must be a single query that returns rows"),
				 errdetail("SQL: %s", sql)));
	if (SPI_keepplan(plan) != 0)
		elog(ERROR, "SPI_keepplan failed");
	if (plan_cache == NULL)
	{
		HASHCTL		ctl;

		ctl.keysize = NAMEDATALEN;
		ctl.entrysize = sizeof(PlanEntry);
		plan_cache = hash_create("pg_knowledge_base plans", 256, &ctl, HASH_ELEM | HASH_STRINGS);
	}
	e = hash_search(plan_cache, key, HASH_ENTER, &found);
	if (found)
	{
		SPI_freeplan(e->plan);
		pfree(e->sql);
	}
	e->sql = MemoryContextStrdup(TopMemoryContext, sql);
	e->plan = plan;
	return plan;
}

/* Plans keep the mappings' tables' shapes: a mapping's change drops them. */
void
kb_mappings_changed(void)
{
	drop_plans();
}

static int
sql_class(Oid type)
{
	switch (getBaseType(type))
	{
		case INT2OID:
		case INT4OID:
		case INT8OID:
		case OIDOID:
			return KB_SQL_INTEGER;
		case NUMERICOID:
			return KB_SQL_DECIMAL;
		case FLOAT4OID:
		case FLOAT8OID:
			return KB_SQL_FLOAT;
		case BOOLOID:
			return KB_SQL_BOOLEAN;
		case DATEOID:
			return KB_SQL_DATE;
		case TIMEOID:
		case TIMETZOID:
			return KB_SQL_TIME;
		case TIMESTAMPOID:
		case TIMESTAMPTZOID:
			return KB_SQL_TIMESTAMP;
		case BYTEAOID:
			return KB_SQL_BINARY;
		default:
			return KB_SQL_STRING;
	}
}

/* A value's lexical form, whatever the session's settings. */
static char *
lexical(Datum value, Oid type, FmgrInfo *out)
{
	char		buf[MAXDATELEN + 64];
	struct pg_tm tm;
	fsec_t		fsec;
	int			tz;

	switch (type)
	{
		case BOOLOID:
			return pstrdup(DatumGetBool(value) ? "true" : "false");
		case FLOAT4OID:
			{
				float4		f = DatumGetFloat4(value);

				if (isnan(f))
					return pstrdup("NaN");
				if (isinf(f))
					return pstrdup(f > 0 ? "INF" : "-INF");
				float_to_shortest_decimal_buf(f, buf);
				return pstrdup(buf);
			}
		case FLOAT8OID:
			{
				float8		f = DatumGetFloat8(value);

				if (isnan(f))
					return pstrdup("NaN");
				if (isinf(f))
					return pstrdup(f > 0 ? "INF" : "-INF");
				double_to_shortest_decimal_buf(f, buf);
				return pstrdup(buf);
			}
		case DATEOID:
			{
				DateADT		d = DatumGetDateADT(value);

				if (DATE_NOT_FINITE(d))
					ereport(ERROR,
							(errcode(ERRCODE_DATETIME_VALUE_OUT_OF_RANGE),
							 errmsg("date out of range"),
							 errdetail("XML Schema dates have no infinity.")));
				j2date(d + POSTGRES_EPOCH_JDATE, &tm.tm_year, &tm.tm_mon, &tm.tm_mday);
				EncodeDateOnly(&tm, USE_XSD_DATES, buf);
				return pstrdup(buf);
			}
		case TIMEOID:
			time2tm(DatumGetTimeADT(value), &tm, &fsec);
			EncodeTimeOnly(&tm, fsec, false, 0, USE_XSD_DATES, buf);
			return pstrdup(buf);
		case TIMETZOID:
			timetz2tm(DatumGetTimeTzADTP(value), &tm, &fsec, &tz);
			EncodeTimeOnly(&tm, fsec, true, tz, USE_XSD_DATES, buf);
			return pstrdup(buf);
		case TIMESTAMPOID:
		case TIMESTAMPTZOID:
			{
				Timestamp	t = DatumGetTimestamp(value);

				if (TIMESTAMP_NOT_FINITE(t))
					ereport(ERROR,
							(errcode(ERRCODE_DATETIME_VALUE_OUT_OF_RANGE),
							 errmsg("timestamp out of range"),
							 errdetail("XML Schema dates have no infinity.")));
				/* with no time zone asked, timestamp2tm() gives UTC */
				if (timestamp2tm(t, NULL, &tm, &fsec, NULL, NULL) != 0)
					ereport(ERROR,
							(errcode(ERRCODE_DATETIME_VALUE_OUT_OF_RANGE),
							 errmsg("timestamp out of range")));
				EncodeDateTime(&tm, fsec, false, 0, NULL, USE_XSD_DATES, buf);
				if (type == TIMESTAMPTZOID)
					strlcat(buf, "Z", sizeof(buf));
				return pstrdup(buf);
			}
		case INTERVALOID:
			{
				struct pg_itm itm;

				interval2itm(*DatumGetIntervalP(value), &itm);
				EncodeInterval(&itm, INTSTYLE_ISO_8601, buf);
				return pstrdup(buf);
			}
		case BYTEAOID:
			{
				bytea	   *b = DatumGetByteaPP(value);
				const unsigned char *p = (const unsigned char *) VARDATA_ANY(b);
				size_t		len = VARSIZE_ANY_EXHDR(b);
				char	   *s = palloc(len * 2 + 1);
				static const char hex[] = "0123456789ABCDEF";
				size_t		i;

				for (i = 0; i < len; i++)
				{
					s[i * 2] = hex[p[i] >> 4];
					s[i * 2 + 1] = hex[p[i] & 15];
				}
				s[len * 2] = '\0';
				return s;
			}
		default:
			return OutputFunctionCall(out, value);
	}
}

/* ---------------------------------------------------------------------
 * The callbacks
 * ---------------------------------------------------------------------
 */

static KbSql *
sql_of(KbCall *call)
{
	if (call->sql == NULL)
		call->sql = MemoryContextAllocZero(call->mcxt, sizeof(KbSql));
	return call->sql;
}

typedef struct DescribeArgs
{
	const char *sql;
	int32	   *ncolumns;
	const KbColumn **columns;
} DescribeArgs;

/*
 * A logical table's SQL is a query (R2RML §5.2), and a mapping's queries
 * open their cursors read-only: what such a cursor refuses -- a statement
 * that writes, a data-modifying WITH, rows locked -- is refused when the
 * mapping is checked, not at its first query.
 */
static void
only_reads(SPIPlanPtr plan, const char *sql)
{
	CachedPlan *cplan = SPI_plan_get_cached_plan(plan);
	ListCell   *lc;

	if (cplan == NULL)
		elog(ERROR, "a logical table's SQL has no plan");
	foreach(lc, cplan->stmt_list)
	{
		PlannedStmt *pstmt = lfirst_node(PlannedStmt, lc);

		if (!CommandIsReadOnly(pstmt))
		{
			const char *command = CreateCommandName((Node *) pstmt);

			ReleaseCachedPlan(cplan, CurrentResourceOwner);
			ereport(ERROR,
					(errcode(ERRCODE_INVALID_OBJECT_DEFINITION),
					 errmsg("a logical table's SQL must only read, and this %s writes or locks rows",
							command),
					 errdetail("SQL: %s", sql)));
		}
	}
	ReleaseCachedPlan(cplan, CurrentResourceOwner);
}

static void
do_sql_describe(KbCall *call, void *arg)
{
	DescribeArgs *a = (DescribeArgs *) arg;
	KbSql	   *s = sql_of(call);
	SPIPlanPtr	plan;
	CachedPlanSource *source;
	TupleDesc	desc;
	MemoryContext old;
	int			i;

	if (SPI_connect() != SPI_OK_CONNECT)
		elog(ERROR, "SPI_connect failed");
	plan = cached_plan(a->sql, 0);
	only_reads(plan, a->sql);
	source = (CachedPlanSource *) linitial(SPI_plan_get_plan_sources(plan));
	desc = source->resultDesc;
	old = MemoryContextSwitchTo(call->mcxt);
	s->ncolumns = desc ? desc->natts : 0;
	s->columns = palloc0(sizeof(KbColumn) * Max(s->ncolumns, 1));
	for (i = 0; i < s->ncolumns; i++)
	{
		Form_pg_attribute att = TupleDescAttr(desc, i);

		s->columns[i].name = kb_str(pstrdup(NameStr(att->attname)));
		s->columns[i].type_oid = att->atttypid;
		s->columns[i].sql_class = sql_class(att->atttypid);
		s->columns[i].type_name = kb_str(format_type_with_typemod(att->atttypid, att->atttypmod));
	}
	MemoryContextSwitchTo(old);
	SPI_finish();
	*a->ncolumns = s->ncolumns;
	*a->columns = s->columns;
}

static int32
host_sql_describe(void *ctx, const char *sql, int32 *ncolumns, const KbColumn **columns)
{
	DescribeArgs a = {sql, ncolumns, columns};

	*ncolumns = 0;
	*columns = NULL;
	return kb_call_guard((KbCall *) ctx, do_sql_describe, &a);
}

static void
sql_cursor_close(KbCursor *base)
{
	SqlCursor  *c = (SqlCursor *) base;

	if (c->base.call->failed)
		return;
	if (c->portal)
	{
		SPI_cursor_close(c->portal);
		c->portal = NULL;
	}
}

typedef struct SqlOpenArgs
{
	const char *sql;
	int32		nparams;
	const KbStr *params;
	KbCursor  **out;
	int32	   *ncolumns;
} SqlOpenArgs;

static void
do_sql_open(KbCall *call, void *arg)
{
	SqlOpenArgs *a = (SqlOpenArgs *) arg;
	SPIPlanPtr	plan;
	Datum	   *values;
	char	   *nulls;
	SqlCursor  *c;
	TupleDesc	desc;
	MemoryContext old;
	int			i;

	kb_call_advance(call);
	KB_TRACE_COUNT(call, KB_TRACE_SQL_QUERIES, 1);
	if (SPI_connect() != SPI_OK_CONNECT)
		elog(ERROR, "SPI_connect failed");
	plan = cached_plan(a->sql, a->nparams);
	old = MemoryContextSwitchTo(call->mcxt);
	values = palloc(sizeof(Datum) * Max(a->nparams, 1));
	nulls = palloc(Max(a->nparams, 1));
	for (i = 0; i < a->nparams; i++)
	{
		values[i] = PointerGetDatum(cstring_to_text_with_len(a->params[i].ptr ? a->params[i].ptr : "",
															 (int) a->params[i].len));
		nulls[i] = ' ';
	}
	c = palloc0(sizeof(SqlCursor));
	c->base.close = sql_cursor_close;
	MemoryContextSwitchTo(old);
	/* read only: the caller's snapshot, and no writes from a mapping */
	c->portal = SPI_cursor_open(NULL, plan, values, nulls, true);
	desc = c->portal->tupDesc;
	old = MemoryContextSwitchTo(call->mcxt);
	c->ncolumns = desc->natts;
	c->types = palloc(sizeof(Oid) * Max(c->ncolumns, 1));
	c->classes = palloc(sizeof(int) * Max(c->ncolumns, 1));
	c->out = palloc(sizeof(FmgrInfo) * Max(c->ncolumns, 1));
	for (i = 0; i < c->ncolumns; i++)
	{
		Form_pg_attribute att = TupleDescAttr(desc, i);
		Oid			outfunc;
		bool		varlena;

		c->types[i] = getBaseType(att->atttypid);
		c->classes[i] = sql_class(att->atttypid);
		getTypeOutputInfo(att->atttypid, &outfunc, &varlena);
		fmgr_info_cxt(outfunc, &c->out[i], call->mcxt);
	}
	c->rowcxt = AllocSetContextCreate(call->mcxt, "pg_knowledge_base rows", ALLOCSET_DEFAULT_SIZES);
	MemoryContextSwitchTo(old);
	SPI_finish();
	kb_call_register(call, &c->base);
	*a->out = &c->base;
	*a->ncolumns = c->ncolumns;
}

static int32
host_sql_open(void *ctx, const char *sql, int32 nparams, const KbStr *params, KbCursor **cursor,
			  int32 *ncolumns)
{
	SqlOpenArgs a = {sql, nparams, params, cursor, ncolumns};

	*cursor = NULL;
	*ncolumns = 0;
	return kb_call_guard((KbCall *) ctx, do_sql_open, &a);
}

typedef struct SqlNextArgs
{
	SqlCursor  *c;
	int32		max;
	const KbSqlValue **values;
	int32	   *nrows;
} SqlNextArgs;

static void
do_sql_next(KbCall *call, void *arg)
{
	SqlNextArgs *a = (SqlNextArgs *) arg;
	SqlCursor  *c = a->c;
	MemoryContext old;
	uint64		n;
	uint64		r;
	int			i;

	(void) call;
	*a->nrows = 0;
	*a->values = NULL;
	if (c->base.closed || c->portal == NULL)
		return;
	MemoryContextReset(c->rowcxt);
	if (SPI_connect() != SPI_OK_CONNECT)
		elog(ERROR, "SPI_connect failed");
	SPI_cursor_fetch(c->portal, true, a->max);
	n = SPI_processed;
	old = MemoryContextSwitchTo(c->rowcxt);
	c->values = palloc0(sizeof(KbSqlValue) * Max(n * c->ncolumns, 1));
	for (r = 0; r < n; r++)
	{
		HeapTuple	tuple = SPI_tuptable->vals[r];

		for (i = 0; i < c->ncolumns; i++)
		{
			KbSqlValue *v = &c->values[r * c->ncolumns + i];
			bool		isnull;
			Datum		value = SPI_getbinval(tuple, SPI_tuptable->tupdesc, i + 1, &isnull);

			v->isnull = isnull;
			if (!isnull)
				v->text = kb_str(lexical(value, c->types[i], &c->out[i]));
		}
	}
	MemoryContextSwitchTo(old);
	SPI_freetuptable(SPI_tuptable);
	SPI_finish();
	*a->values = c->values;
	*a->nrows = (int32) n;
	KB_TRACE_COUNT(c->base.call, KB_TRACE_SQL_ROWS, (int64) n);
}

static int32
host_sql_next(KbCursor *cursor, int32 max, const KbSqlValue **values, int32 *nrows)
{
	SqlNextArgs a = {(SqlCursor *) cursor, max, values, nrows};

	*nrows = 0;
	*values = NULL;
	return kb_call_guard(cursor->call, do_sql_next, &a);
}

static void
do_sql_close(KbCall *call, void *arg)
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
host_sql_close(KbCursor *cursor)
{
	if (cursor == NULL || cursor->closed || cursor->call->failed)
		return;
	(void) kb_call_guard(cursor->call, do_sql_close, cursor);
}

typedef struct MappingsArgs
{
	int32	   *n;
	const KbMappingRow **rows;
} MappingsArgs;

static void
do_mappings_get(KbCall *call, void *arg)
{
	MappingsArgs *a = (MappingsArgs *) arg;
	KbSql	   *s = sql_of(call);
	MemoryContext old;
	uint64		i;
	int			rc;

	kb_call_advance(call);
	if (SPI_connect() != SPI_OK_CONNECT)
		elog(ERROR, "SPI_connect failed");
	rc = SPI_execute("SELECT name, document, coalesce(base_iri, ''), revision FROM "
					 KB_SCHEMA ".mappings WHERE enabled ORDER BY name", true, 0);
	if (rc != SPI_OK_SELECT)
		elog(ERROR, "reading %s.mappings failed: %s", KB_SCHEMA, SPI_result_code_string(rc));
	old = MemoryContextSwitchTo(call->mcxt);
	s->nmappings = (int) SPI_processed;
	s->mappings = palloc0(sizeof(KbMappingRow) * Max(s->nmappings, 1));
	for (i = 0; i < SPI_processed; i++)
	{
		HeapTuple	tuple = SPI_tuptable->vals[i];
		TupleDesc	desc = SPI_tuptable->tupdesc;
		bool		isnull;

		s->mappings[i].name = kb_str(SPI_getvalue(tuple, desc, 1));
		s->mappings[i].document = kb_str(SPI_getvalue(tuple, desc, 2));
		s->mappings[i].base_iri = kb_str(SPI_getvalue(tuple, desc, 3));
		s->mappings[i].revision = DatumGetInt64(SPI_getbinval(tuple, desc, 4, &isnull));
	}
	MemoryContextSwitchTo(old);
	SPI_finish();
	*a->n = s->nmappings;
	*a->rows = s->mappings;
}

static int32
host_mappings_get(void *ctx, int32 *n, const KbMappingRow **rows)
{
	MappingsArgs a = {n, rows};

	*n = 0;
	*rows = NULL;
	return kb_call_guard((KbCall *) ctx, do_mappings_get, &a);
}

void
kb_sql_host(KbHost *host)
{
	host->sql_describe = host_sql_describe;
	host->sql_open = host_sql_open;
	host->sql_next = host_sql_next;
	host->sql_close = host_sql_close;
	host->mappings_get = host_mappings_get;
}

void
kb_sql_close(KbCall *call)
{
	call->sql = NULL;
}
