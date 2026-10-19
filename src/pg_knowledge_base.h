/* SPDX-License-Identifier: Apache-2.0 */
/*-------------------------------------------------------------------------
 *
 * pg_knowledge_base.h
 *	  pg_knowledge_base: oxigraph's SPARQL engine in PostgreSQL 19, its RDF
 *	  store in PostgreSQL's tables, R2RML's virtual graphs over PostgreSQL's
 *	  tables, and a SPARQL 1.1 endpoint (docs/design.md).
 *
 * The parts:
 *
 *	module.c		_PG_init: the settings, the endpoint's registration
 *	engine.c		libkb_engine.so loaded on first use, its errors raised as
 *					PostgreSQL's
 *	call.c			one call of the engine: its host (KbHost), the errors its
 *					callbacks keep, the cursors they open, the output
 *	store.c			the store: index-only scans of kb.quads, the
 *					dictionary kb.terms, the named graphs kb.graphs, and
 *					writes to them
 *	sql.c			SQL for R2RML: logical tables described and read, values
 *					as R2RML's lexical forms; kb.mappings
 *	convert.c		RDF terms as the column types of kb.sparql()'s rows
 *	functions.c		the SQL functions
 *	endpoint/		the SPARQL endpoint: an acceptor, a session a connection
 *
 *-------------------------------------------------------------------------
 */
#ifndef PG_KNOWLEDGE_BASE_H
#define PG_KNOWLEDGE_BASE_H

#include "access/htup.h"
#include "lib/stringinfo.h"
#include "nodes/pg_list.h"
#include "portability/instr_time.h"
#include "utils/relcache.h"

#include "kb_engine.h"

#define KB_VERSION			"0.1.0"
#define KB_SCHEMA			"kb"

/* module.c: the settings */
extern char *kb_engine_library;
extern bool kb_union_default_graph;
extern bool kb_with_mappings;
extern int	kb_load_batch;
extern int	kb_engine_memory_limit;
extern int	kb_term_cache_entries;

/* engine.c */
extern const KbApi *kb_api(void);
extern bool kb_engine_loaded(void);
pg_noreturn extern void kb_raise(KbError *err, const char *context);
extern char *kb_take_string(char *s);
extern int	kb_sqlstate(int32 code);

/*
 * One call of the engine (call.c): the host the engine is given, and what
 * its callbacks keep.  A callback that catches PostgreSQL's error keeps it
 * here and fails; every later callback fails at once, and closes do
 * nothing, until kb_call_finish() re-throws it once the engine has
 * returned.
 */
typedef struct KbStore KbStore;
typedef struct KbSql KbSql;
typedef struct KbTraceStmt KbTraceStmt;

typedef struct KbCall
{
	KbHost		host;			/* host.ctx is the call */
	MemoryContext mcxt;			/* the call's own memory */
	MemoryContext caller;		/* where the kept error is copied */
	ErrorData  *error;			/* kept, re-thrown by kb_call_finish() */
	bool		failed;
	List	   *cursors;		/* KbCursor *, open */
	KbStore    *store;			/* store.c's: relations and scans */
	KbSql	   *sql;			/* sql.c's: plans and mapping rows */
	bool		writes;			/* the call may write: reads see its writes */
	bool		bulk;			/* a bulk load's: quads go to its staging table */
	KbTraceStmt *trace;			/* its statement's record, when traced (trace.c) */
	bool		dirty;			/* written since the snapshot's command id */
	bool		pushed_snapshot;
	ErrorContextCallback errcb; /* the call's context, on its errors */
	bool		has_context;
	/* output: write() into out, or through write_fn */
	StringInfo	out;
	bool		(*write_fn) (void *arg, const char *data, size_t len);
	void	   *write_arg;
	int32		(*respond_fn) (void *arg, int32 status, const char *content_type,
							   const char *headers);
} KbCall;

/* A cursor of the call: its kind's close, run once */
struct KbCursor
{
	struct KbCall *call;
	void		(*close) (struct KbCursor *cursor);
	bool		closed;
};

extern KbCall *kb_call_begin(bool writes, const char *context);
extern void kb_call_finish(KbCall *call);
extern void kb_call_abort(KbCall *call);
extern void kb_call_check(KbCall *call, int32 rc, KbError *err);
extern void kb_call_register(KbCall *call, KbCursor *cursor);
extern void kb_call_advance(KbCall *call);
extern int32 kb_call_guard(KbCall *call, void (*fn) (KbCall *call, void *arg), void *arg);

/* store.c */
extern void kb_store_host(KbHost *host);
extern void kb_store_close(KbCall *call);
extern Oid	kb_relid(const char *name);
typedef struct KbBulk KbBulk;
extern KbBulk *kb_bulk_begin(void);
extern void kb_bulk_finish(KbBulk *bulk);

/* trace.c: pg_otel_probe's spans */
typedef enum KbTraceCounter
{
	KB_TRACE_SCANS,
	KB_TRACE_QUADS_READ,
	KB_TRACE_TERMS_LOOKED_UP,
	KB_TRACE_QUADS_WRITTEN,
	KB_TRACE_QUADS_DELETED,
	KB_TRACE_TERMS_WRITTEN,
	KB_TRACE_SQL_QUERIES,
	KB_TRACE_SQL_ROWS,
	KB_TRACE_LOADED,
	KB_TRACE_BULK
} KbTraceCounter;

/* A call's trace, on its caller's stack: stmt NULL when it is not traced */
typedef struct KbTraceCall
{
	KbTraceStmt *stmt;
	instr_time	start;
} KbTraceCall;

extern void kb_trace_define_settings(void);
extern void kb_trace_init(void);
extern bool kb_trace_probe_loaded(void);
extern bool kb_trace_wants_plan(void);
extern void kb_trace_begin(KbCall *call, KbTraceCall *tc);
extern void kb_trace_end(KbTraceCall *tc, const char *operation, KbStr text, int64 rows,
						 KbQuery *q, KbHttp *http);
extern KbTraceStmt *kb_trace_current(void);
extern void kb_trace_http(KbTraceStmt *st, const char *method, const char *route, int status);
extern void kb_trace_count(KbTraceStmt *st, KbTraceCounter counter, int64 n);

/* A traced call's counter */
#define KB_TRACE_COUNT(call, counter, n) \
	do { \
		if ((call)->trace != NULL) \
			kb_trace_count((call)->trace, (counter), (n)); \
	} while (0)

/* sql.c */
extern void kb_sql_host(KbHost *host);
extern void kb_sql_close(KbCall *call);
extern void kb_mappings_changed(void);

/* convert.c */
extern Datum kb_term_datum(const KbTerm *term, Oid typid, int32 typmod, bool *isnull,
						   const char *column);
extern char *kb_term_json(const KbTerm *term);

/* KbStr helpers */
static inline KbStr
kb_str(const char *s)
{
	KbStr		r;

	r.ptr = s;
	r.len = s ? (int64_t) strlen(s) : 0;
	return r;
}

static inline KbStr
kb_str_len(const char *s, size_t len)
{
	KbStr		r;

	r.ptr = s;
	r.len = (int64_t) len;
	return r;
}

/* A KbStr as a palloc'd C string */
static inline char *
kb_cstring(KbStr s)
{
	return pnstrdup(s.ptr ? s.ptr : "", (Size) s.len);
}

#endif							/* PG_KNOWLEDGE_BASE_H */
