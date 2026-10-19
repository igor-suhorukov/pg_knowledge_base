/* SPDX-License-Identifier: Apache-2.0 */
/*-------------------------------------------------------------------------
 *
 * trace.c
 *	  pg_knowledge_base in pg_otel_probe's traces (docs/design.md §2.8): the
 *	  SPARQL a statement ran, as attributes of the probe's span of that
 *	  statement -- the operation, the text, the plan with each node's
 *	  results and time, the rows, the engine's time and memory, the store's
 *	  scans and writes, the SQL of R2RML's mappings -- through the probe's
 *	  statement reporters (include/pg_otel_probe_api.h, the probe's PLAN.md
 *	  §4.7.4).
 *
 * Neither module links the other: the reporter is registered in the
 * probe's registry, made by whichever of the two loads first, and the
 * probe's context functions are looked up by name, NULL where it is not
 * loaded.  Without the probe, nothing here does any work but the hooks'
 * bookkeeping.
 *
 * Which statement a call is of.  A reporter is called with a statement's
 * QueryDesc, in ExecutorEnd; a SPARQL function is called, as any function,
 * with no pointer to the statement it runs in.  So ExecutorRun and
 * ExecutorFinish keep the statements under way, innermost last, and a call
 * belongs to the innermost one: the SELECT that calls kb.sparql(), or the
 * endpoint's statement of a request (session.c).  A call is traced when the
 * probe has a span for that statement (its context function outgoing());
 * its figures go to a record of the statement, which ExecutorEnd drops once
 * the probe has read it -- whichever of the two modules' hooks runs first,
 * the probe reads before the executor ends -- and the end of the
 * transaction, or of the subtransaction that made it, drops with the rest.
 *
 * A statement's several calls -- kb.sparql() in a join, in a LATERAL, in a
 * function called per row -- are added up, and the longest of them names
 * the operation, the text and the plan.  The probe gives an extension's
 * attributes PG_OTEL_PROBE_ATTRS_BYTES (4 kB) a span, shared with the other
 * extensions': the text gets at most TEXT_MAX of it, the plan PLAN_MAX.
 *
 * The plan is spareval's explanation (kb.sparql_explain()'s), which needs
 * the query evaluated with statistics -- a timer around every node's every
 * row: it is asked for only when the statement is traced, and
 * kb.trace_plan turns it off.  Literals in the text and in the plan's
 * labels are replaced by '?' unless kb.trace_query_text is raw; by default
 * the setting follows pg_otel_probe.query_text, whose off it honours too.
 *
 *-------------------------------------------------------------------------
 */
#include "postgres.h"

#include "access/xact.h"
#include "executor/executor.h"
#include "miscadmin.h"
#include "portability/instr_time.h"
#include "utils/guc.h"
#include "utils/memutils.h"

#include "pg_knowledge_base.h"
#include "pg_otel_probe_api.h"

#define TEXT_MAX		1024
#define PLAN_MAX		2048
#define MAX_RUNNING		64
#define MAX_RECORDS		32

typedef enum TraceText
{
	TRACE_TEXT_INHERIT,
	TRACE_TEXT_OFF,
	TRACE_TEXT_NORMALIZED,
	TRACE_TEXT_RAW
} TraceText;

static const struct config_enum_entry trace_text_options[] = {
	{"inherit", TRACE_TEXT_INHERIT, false},
	{"off", TRACE_TEXT_OFF, false},
	{"normalized", TRACE_TEXT_NORMALIZED, false},
	{"raw", TRACE_TEXT_RAW, false},
	{NULL, 0, false}
};

static int	kb_trace_query_text = TRACE_TEXT_INHERIT;
static bool kb_trace_plan = true;

/* A statement's SPARQL, for its span */
struct KbTraceStmt
{
	QueryDesc  *qd;
	SubTransactionId subxid;
	int64		calls;
	int64		rows;
	double		ms;
	int64		memory_peak;
	int64		scans;
	int64		quads_read;
	int64		terms_looked_up;
	int64		quads_written;
	int64		quads_deleted;
	int64		terms_written;
	int64		sql_queries;
	int64		sql_rows;
	int64		loaded;
	bool		bulk;
	/* the longest call's */
	double		main_ms;
	char		operation[16];
	char	   *text;
	char	   *plan;
	int32		plan_nodes;
	/* an endpoint request's */
	char		http_method[16];
	char		http_route[16];
	int32		http_status;
};

static bool hooked = false;
static ExecutorRun_hook_type prev_ExecutorRun = NULL;
static ExecutorFinish_hook_type prev_ExecutorFinish = NULL;
static ExecutorEnd_hook_type prev_ExecutorEnd = NULL;

static QueryDesc *running[MAX_RUNNING];
static int	depth = 0;

static KbTraceStmt *records[MAX_RECORDS];
static int	nrecords = 0;
static MemoryContext trace_cxt = NULL;

/* ---------------------------------------------------------------------
 * The probe
 * ---------------------------------------------------------------------
 */

/* The probe's context functions, looked up once a process, after its fork */
static const PgOtelProbeContextApi *
probe(void)
{
	static int	looked_up = 0;
	static const PgOtelProbeContextApi *found = NULL;

	if (looked_up != MyProcPid)
	{
		found = pg_otel_probe_context_api();
		looked_up = MyProcPid;
	}
	return found;
}

bool
kb_trace_probe_loaded(void)
{
	return hooked && probe() != NULL;
}

/* Whether the probe has a span for the statement under way */
static bool
traced(void)
{
	const PgOtelProbeContextApi *otel;
	char		traceparent[PG_OTEL_PROBE_TRACEPARENT_LEN + 1];

	if (!hooked || depth == 0 || depth > MAX_RUNNING)
		return false;
	otel = probe();
	return otel != NULL && otel->outgoing(traceparent, NULL, 0);
}

static TraceText
text_mode(void)
{
	const char *probe_mode;

	if (kb_trace_query_text != TRACE_TEXT_INHERIT)
		return (TraceText) kb_trace_query_text;
	probe_mode = GetConfigOption("pg_otel_probe.query_text", true, false);
	if (probe_mode != NULL && strcmp(probe_mode, "off") == 0)
		return TRACE_TEXT_OFF;
	if (probe_mode != NULL && strcmp(probe_mode, "raw") == 0)
		return TRACE_TEXT_RAW;
	return TRACE_TEXT_NORMALIZED;
}

/* ---------------------------------------------------------------------
 * The records
 * ---------------------------------------------------------------------
 */

static KbTraceStmt *
find(QueryDesc *qd)
{
	int			i;

	for (i = 0; i < nrecords; i++)
		if (records[i]->qd == qd)
			return records[i];
	return NULL;
}

static void
forget(int i)
{
	records[i] = records[--nrecords];
}

/* The record of the statement under way, made at its first call */
static KbTraceStmt *
current(void)
{
	QueryDesc  *qd = running[depth - 1];
	KbTraceStmt *st = find(qd);

	if (st != NULL || nrecords >= MAX_RECORDS)
		return st;
	if (trace_cxt == NULL)
		trace_cxt = AllocSetContextCreate(TopMemoryContext, "pg_knowledge_base traces",
										  ALLOCSET_SMALL_SIZES);
	st = MemoryContextAllocZero(trace_cxt, sizeof(KbTraceStmt));
	st->qd = qd;
	st->subxid = GetCurrentSubTransactionId();
	records[nrecords++] = st;
	return st;
}

/* Whether a query should be evaluated with statistics, for its plan */
bool
kb_trace_wants_plan(void)
{
	return kb_trace_plan && traced();
}

/* A call begun: traced, its figures go to its statement's record */
void
kb_trace_begin(KbCall *call, KbTraceCall *tc)
{
	tc->stmt = traced() ? current() : NULL;
	call->trace = tc->stmt;
	if (tc->stmt == NULL)
		return;
	tc->stmt->calls++;
	INSTR_TIME_SET_CURRENT(tc->start);
	(void) kb_api()->memory_peak(true);
}

static char *
keep(const char *s)
{
	return s != NULL ? MemoryContextStrdup(trace_cxt, s) : NULL;
}

/*
 * A traced call's end: its figures added to its statement's, and its
 * operation, text and plan the statement's if it is the longest yet.  q or
 * http, when given, says the operation, the rows and the plan; else the
 * operation and rows given do.  It raises nothing: what the engine cannot
 * give is left out.
 */
void
kb_trace_end(KbTraceCall *tc, const char *operation, KbStr text, int64 rows,
			 KbQuery *q, KbHttp *http)
{
	const KbApi *api = kb_api();
	KbTraceStmt *st = tc->stmt;
	KbTrace		t;
	KbError		err = {0};
	instr_time	now;
	double		ms;
	TraceText	mode;
	bool		raw;
	int32		rc = KB_OK;

	if (st == NULL)
		return;
	tc->stmt = NULL;
	INSTR_TIME_SET_CURRENT(now);
	INSTR_TIME_SUBTRACT(now, tc->start);
	ms = INSTR_TIME_GET_MILLISEC(now);
	st->ms += ms;
	st->memory_peak = Max(st->memory_peak, api->memory_peak(false));
	mode = text_mode();
	raw = mode == TRACE_TEXT_RAW;

	memset(&t, 0, sizeof(t));
	t.size = sizeof(t);
	if (q != NULL)
		rc = api->query_trace(q, PLAN_MAX, raw, &t, &err);
	else if (http != NULL)
		rc = api->http_trace(http, PLAN_MAX, raw, &t, &err);
	if (rc != KB_OK)
	{
		if (err.message)
			api->free_string(err.message);
		memset(&t, 0, sizeof(t));
	}
	if (t.operation[0] != '\0')
		operation = t.operation;
	st->rows += (q != NULL || http != NULL) ? t.rows : rows;

	if (st->calls == 1 || ms >= st->main_ms)
	{
		char	   *normalized = NULL;

		st->main_ms = ms;
		strlcpy(st->operation, operation ? operation : "", sizeof(st->operation));
		if (st->text)
			pfree(st->text);
		st->text = NULL;
		if (mode != TRACE_TEXT_OFF && text.ptr != NULL && text.len > 0)
		{
			memset(&err, 0, sizeof(err));
			if (api->text_trace(text, TEXT_MAX, raw, &normalized, &err) == KB_OK)
				st->text = keep(normalized);
			else if (err.message)
				api->free_string(err.message);
			if (normalized)
				api->free_string(normalized);
		}
		if (st->plan)
			pfree(st->plan);
		st->plan = keep(t.plan);
		st->plan_nodes = t.plan_nodes;
	}
	if (t.plan)
		api->free_string(t.plan);
}

/* An endpoint request's method, route and status, on its statement's span */
void
kb_trace_http(KbTraceStmt *st, const char *method, const char *route, int status)
{
	if (st == NULL)
		return;
	strlcpy(st->http_method, method, sizeof(st->http_method));
	strlcpy(st->http_route, route, sizeof(st->http_route));
	st->http_status = status;
}

KbTraceStmt *
kb_trace_current(void)
{
	return traced() ? current() : NULL;
}

void
kb_trace_count(KbTraceStmt *st, KbTraceCounter counter, int64 n)
{
	if (st == NULL)
		return;
	switch (counter)
	{
		case KB_TRACE_SCANS:
			st->scans += n;
			break;
		case KB_TRACE_QUADS_READ:
			st->quads_read += n;
			break;
		case KB_TRACE_TERMS_LOOKED_UP:
			st->terms_looked_up += n;
			break;
		case KB_TRACE_QUADS_WRITTEN:
			st->quads_written += n;
			break;
		case KB_TRACE_QUADS_DELETED:
			st->quads_deleted += n;
			break;
		case KB_TRACE_TERMS_WRITTEN:
			st->terms_written += n;
			break;
		case KB_TRACE_SQL_QUERIES:
			st->sql_queries += n;
			break;
		case KB_TRACE_SQL_ROWS:
			st->sql_rows += n;
			break;
		case KB_TRACE_LOADED:
			st->loaded += n;
			break;
		case KB_TRACE_BULK:
			st->bulk = true;
			break;
	}
}

/* ---------------------------------------------------------------------
 * The reporter: a statement's record, as its span's attributes
 * ---------------------------------------------------------------------
 */

static void
add_count(PgOtelProbeAttrs *attrs, const char *key, int64 n)
{
	if (n != 0)
		attrs->add_int(attrs, key, n);
}

/*
 * It reads the record alone and allocates nothing, so it is safe on a
 * failed statement's error path, where the probe calls it with the
 * figures the statement got to.
 */
static void
kb_statement_report(QueryDesc *queryDesc, PgOtelProbeAttrs *attrs, int when)
{
	KbTraceStmt *st = find(queryDesc);

	(void) when;
	if (st == NULL)
		return;
	attrs->add_int(attrs, "kb.sparql.calls", st->calls);
	if (st->operation[0] != '\0')
		attrs->add_string(attrs, "kb.sparql.operation", st->operation);
	if (st->text != NULL)
		attrs->add_string(attrs, "kb.sparql.text", st->text);
	if (st->plan != NULL)
	{
		attrs->add_string(attrs, "kb.sparql.plan", st->plan);
		attrs->add_int(attrs, "kb.sparql.plan_nodes", st->plan_nodes);
	}
	attrs->add_int(attrs, "kb.sparql.rows", st->rows);
	attrs->add_double(attrs, "kb.sparql.duration_ms", st->ms);
	attrs->add_int(attrs, "kb.engine.memory_peak_bytes", st->memory_peak);
	add_count(attrs, "kb.store.scans", st->scans);
	add_count(attrs, "kb.store.quads_read", st->quads_read);
	add_count(attrs, "kb.store.terms_looked_up", st->terms_looked_up);
	add_count(attrs, "kb.store.quads_written", st->quads_written);
	add_count(attrs, "kb.store.quads_deleted", st->quads_deleted);
	add_count(attrs, "kb.store.terms_written", st->terms_written);
	add_count(attrs, "kb.r2rml.queries", st->sql_queries);
	add_count(attrs, "kb.r2rml.rows", st->sql_rows);
	add_count(attrs, "kb.load.quads", st->loaded);
	if (st->bulk)
		attrs->add_bool(attrs, "kb.load.bulk", true);
	if (st->http_method[0] != '\0')
	{
		attrs->add_string(attrs, "kb.http.request.method", st->http_method);
		attrs->add_string(attrs, "kb.http.route", st->http_route);
		if (st->http_status > 0)
			attrs->add_int(attrs, "kb.http.response.status_code", st->http_status);
	}
}

/* ---------------------------------------------------------------------
 * The hooks: the statements under way, and their records' end
 * ---------------------------------------------------------------------
 */

static void
kb_ExecutorRun(QueryDesc *queryDesc, ScanDirection direction, uint64 count)
{
	int			saved = depth;

	if (depth < MAX_RUNNING)
		running[depth] = queryDesc;
	depth++;
	PG_TRY();
	{
		if (prev_ExecutorRun)
			prev_ExecutorRun(queryDesc, direction, count);
		else
			standard_ExecutorRun(queryDesc, direction, count);
	}
	PG_FINALLY();
	{
		depth = saved;
	}
	PG_END_TRY();
}

static void
kb_ExecutorFinish(QueryDesc *queryDesc)
{
	int			saved = depth;

	if (depth < MAX_RUNNING)
		running[depth] = queryDesc;
	depth++;
	PG_TRY();
	{
		if (prev_ExecutorFinish)
			prev_ExecutorFinish(queryDesc);
		else
			standard_ExecutorFinish(queryDesc);
	}
	PG_FINALLY();
	{
		depth = saved;
	}
	PG_END_TRY();
}

/* The probe reads the record before the executor ends; then it goes. */
static void
kb_ExecutorEnd(QueryDesc *queryDesc)
{
	int			i;

	if (prev_ExecutorEnd)
		prev_ExecutorEnd(queryDesc);
	else
		standard_ExecutorEnd(queryDesc);
	for (i = 0; i < nrecords; i++)
		if (records[i]->qd == queryDesc)
		{
			forget(i);
			break;
		}
}

static void
trace_xact_callback(XactEvent event, void *arg)
{
	(void) arg;
	switch (event)
	{
		case XACT_EVENT_COMMIT:
		case XACT_EVENT_PARALLEL_COMMIT:
		case XACT_EVENT_ABORT:
		case XACT_EVENT_PARALLEL_ABORT:
		case XACT_EVENT_PREPARE:
			nrecords = 0;
			if (trace_cxt != NULL)
				MemoryContextReset(trace_cxt);
			break;
		default:
			break;
	}
}

static void
trace_subxact_callback(SubXactEvent event, SubTransactionId mySubid,
					   SubTransactionId parentSubid, void *arg)
{
	int			i;

	(void) parentSubid;
	(void) arg;
	if (event != SUBXACT_EVENT_ABORT_SUB)
		return;
	for (i = nrecords - 1; i >= 0; i--)
		if (records[i]->subxid >= mySubid)
			forget(i);
}

void
kb_trace_define_settings(void)
{
	DefineCustomEnumVariable("kb.trace_query_text",
							 "The SPARQL text on pg_otel_probe's spans: inherit, off, normalized, raw.",
							 "normalized replaces literals by '?', in the plan's labels too; inherit "
							 "follows pg_otel_probe.query_text: off off, raw raw, any other normalized.",
							 &kb_trace_query_text,
							 TRACE_TEXT_INHERIT, trace_text_options,
							 PGC_SUSET, 0,
							 NULL, NULL, NULL);
	DefineCustomBoolVariable("kb.trace_plan",
							 "SPARQL plans, each node with its results and time, on pg_otel_probe's spans.",
							 "A traced query is then evaluated with spareval's statistics.",
							 &kb_trace_plan,
							 true,
							 PGC_SUSET, 0,
							 NULL, NULL, NULL);
}

/* From _PG_init, while shared_preload_libraries are loaded */
void
kb_trace_init(void)
{
	pg_otel_probe_register_statement_reporter(kb_statement_report, PG_OTEL_PROBE_ABORT_SAFE);
	prev_ExecutorRun = ExecutorRun_hook;
	ExecutorRun_hook = kb_ExecutorRun;
	prev_ExecutorFinish = ExecutorFinish_hook;
	ExecutorFinish_hook = kb_ExecutorFinish;
	prev_ExecutorEnd = ExecutorEnd_hook;
	ExecutorEnd_hook = kb_ExecutorEnd;
	RegisterXactCallback(trace_xact_callback, NULL);
	RegisterSubXactCallback(trace_subxact_callback, NULL);
	hooked = true;
}
