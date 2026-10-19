/* SPDX-License-Identifier: Apache-2.0 */
/*-------------------------------------------------------------------------
 *
 * functions.c
 *	  The SQL functions (docs/design.md §2.3, sql/pg_knowledge_base--0.1.sql).
 *
 *	kb.sparql(query, ...)			SETOF record: the solutions as rows,
 *									columns matched to variables by name
 *	kb.sparql_json(query, ...)		jsonb: the results as oxigraph's server
 *									answers them, SPARQL's JSON results or,
 *									for a graph, JSON-LD
 *	kb.sparql_text(query, format)	text: the results in any format
 *	kb.sparql_explain(query)		jsonb: oxigraph's plan, with statistics
 *	kb.sparql_update(update)		an update, in the caller's transaction
 *	kb.load(), kb.load_file()		RDF documents into the store
 *	kb.dump()						the store as a document
 *	kb.term(), kb.term_key()		a key's term, a term's key
 *	kb.add_mapping(), kb.drop_mapping(), kb.direct_mapping()
 *									R2RML mappings
 *
 *-------------------------------------------------------------------------
 */
#include "postgres.h"

#include "access/htup_details.h"
#include "catalog/pg_authid.h"
#include "catalog/pg_type.h"
#include "executor/spi.h"
#include "funcapi.h"
#include "miscadmin.h"
#include "tcop/utility.h"
#include "utils/acl.h"
#include "utils/array.h"
#include "utils/builtins.h"
#include "utils/jsonb.h"
#include "utils/lsyscache.h"
#include "utils/memutils.h"
#include "utils/regproc.h"
#include "utils/tuplestore.h"
#include "utils/uuid.h"

#include "pg_knowledge_base.h"

PG_FUNCTION_INFO_V1(kb_sparql);
PG_FUNCTION_INFO_V1(kb_sparql_json);
PG_FUNCTION_INFO_V1(kb_sparql_text);
PG_FUNCTION_INFO_V1(kb_sparql_explain);
PG_FUNCTION_INFO_V1(kb_sparql_update);
PG_FUNCTION_INFO_V1(kb_load);
PG_FUNCTION_INFO_V1(kb_load_file);
PG_FUNCTION_INFO_V1(kb_dump);
PG_FUNCTION_INFO_V1(kb_term);
PG_FUNCTION_INFO_V1(kb_term_key);
PG_FUNCTION_INFO_V1(kb_check_mapping);
PG_FUNCTION_INFO_V1(kb_direct_mapping_json);
PG_FUNCTION_INFO_V1(kb_engine_version);
PG_FUNCTION_INFO_V1(kb_mappings_changed_trigger);

static char *
opt_text(FunctionCallInfo fcinfo, int arg)
{
	if (PG_NARGS() <= arg || PG_ARGISNULL(arg))
		return NULL;
	return text_to_cstring(PG_GETARG_TEXT_PP(arg));
}

/* A text[] argument as KbStr's; NULL elements refused. */
static KbStr *
opt_text_array(FunctionCallInfo fcinfo, int arg, int32 *n)
{
	ArrayType  *array;
	Datum	   *elems;
	bool	   *nulls;
	int			nelems;
	KbStr	   *out;
	int			i;

	*n = 0;
	if (PG_NARGS() <= arg || PG_ARGISNULL(arg))
		return NULL;
	array = PG_GETARG_ARRAYTYPE_P(arg);
	deconstruct_array_builtin(array, TEXTOID, &elems, &nulls, &nelems);
	out = palloc(sizeof(KbStr) * Max(nelems, 1));
	for (i = 0; i < nelems; i++)
	{
		if (nulls[i])
			ereport(ERROR,
					(errcode(ERRCODE_NULL_VALUE_NOT_ALLOWED),
					 errmsg("a graph name is NULL")));
		out[i] = kb_str(TextDatumGetCString(elems[i]));
	}
	*n = nelems;
	return out;
}

/* The spec of a query from the functions' common arguments. */
static void
query_spec(FunctionCallInfo fcinfo, KbQuerySpec *spec, int base_arg, int graphs_arg)
{
	char	   *query;
	char	   *base;

	if (PG_ARGISNULL(0))
		ereport(ERROR,
				(errcode(ERRCODE_NULL_VALUE_NOT_ALLOWED),
				 errmsg("the query is NULL")));
	query = text_to_cstring(PG_GETARG_TEXT_PP(0));
	base = base_arg >= 0 ? opt_text(fcinfo, base_arg) : NULL;
	memset(spec, 0, sizeof(*spec));
	spec->size = sizeof(KbQuerySpec);
	spec->query = kb_str(query);
	spec->base_iri = kb_str(base);
	if (graphs_arg >= 0)
	{
		spec->default_graphs = opt_text_array(fcinfo, graphs_arg, &spec->ndefault_graphs);
		spec->named_graphs = opt_text_array(fcinfo, graphs_arg + 1, &spec->nnamed_graphs);
	}
	spec->union_default_graph = kb_union_default_graph;
	spec->with_mappings = kb_with_mappings;
}

/* The engine's error context: the start of the query or update */
static char *
statement_context(const char *what, KbStr text)
{
	int			len = (int) Min(text.len, 200);

	return psprintf("%s \"%.*s%s\"", what, len, text.ptr ? text.ptr : "",
					text.len > 200 ? "..." : "");
}

/*
 * kb.sparql(query text, base_iri text, default_graphs text[], named_graphs text[])
 * RETURNS SETOF record
 */
Datum
kb_sparql(PG_FUNCTION_ARGS)
{
	ReturnSetInfo *rsinfo = (ReturnSetInfo *) fcinfo->resultinfo;
	const KbApi *api = kb_api();
	KbQuerySpec spec;
	KbError		err = {0};
	KbCall	   *call;
	KbQuery    *volatile q = NULL;
	TupleDesc	desc;
	int		   *map;
	int32		nvars;
	const KbStr *names;
	int			i;
	int32		rc;
	char	   *context;
	MemoryContext rowcxt;
	KbTraceCall tc;

	query_spec(fcinfo, &spec, 1, 2);
	spec.explain = kb_trace_wants_plan();
	context = statement_context("SPARQL query", spec.query);
	InitMaterializedSRF(fcinfo, MAT_SRF_USE_EXPECTED_DESC);
	desc = rsinfo->setDesc;

	call = kb_call_begin(false, context);
	kb_trace_begin(call, &tc);
	rc = api->query_open(&call->host, &spec, (KbQuery **) &q, &err);
	kb_call_check(call, rc, &err);
	PG_TRY();
	{
		api->query_variables(q, &nvars, &names);
		/* each column its variable: by name, then without regard to case */
		map = palloc(sizeof(int) * Max(desc->natts, 1));
		for (i = 0; i < desc->natts; i++)
		{
			Form_pg_attribute att = TupleDescAttr(desc, i);
			const char *col = NameStr(att->attname);
			int			v;

			map[i] = -1;
			if (att->attisdropped)
				continue;
			for (v = 0; v < nvars && map[i] < 0; v++)
				if ((size_t) names[v].len == strlen(col) && memcmp(names[v].ptr, col, names[v].len) == 0)
					map[i] = v;
			for (v = 0; v < nvars && map[i] < 0; v++)
				if ((size_t) names[v].len == strlen(col) &&
					pg_strncasecmp(names[v].ptr, col, names[v].len) == 0)
					map[i] = v;
			if (map[i] < 0)
			{
				StringInfoData vars;

				initStringInfo(&vars);
				for (v = 0; v < nvars; v++)
					appendStringInfo(&vars, "%s?%.*s", v ? ", " : "", (int) names[v].len, names[v].ptr);
				ereport(ERROR,
						(errcode(ERRCODE_UNDEFINED_COLUMN),
						 errmsg("column \"%s\" names no variable of the SPARQL query", col),
						 errdetail("The query's variables: %s.", nvars ? vars.data : "none")));
			}
		}

		rowcxt = AllocSetContextCreate(CurrentMemoryContext, "kb.sparql row", ALLOCSET_DEFAULT_SIZES);
		for (;;)
		{
			const KbTerm *values;
			bool		done;
			Datum	   *datums;
			bool	   *nulls;
			MemoryContext old;

			rc = api->query_next(q, &values, &done, &err);
			if (rc != KB_OK)
			{
				/* after a host's error the callbacks of the close do nothing */
				api->query_close(q);
				q = NULL;
				kb_call_check(call, rc, &err);
			}
			if (done)
				break;
			old = MemoryContextSwitchTo(rowcxt);
			datums = palloc(sizeof(Datum) * Max(desc->natts, 1));
			nulls = palloc(sizeof(bool) * Max(desc->natts, 1));
			for (i = 0; i < desc->natts; i++)
			{
				Form_pg_attribute att = TupleDescAttr(desc, i);

				if (map[i] < 0)
				{
					nulls[i] = true;
					continue;
				}
				datums[i] = kb_term_datum(&values[map[i]], att->atttypid, att->atttypmod,
										  &nulls[i], NameStr(att->attname));
			}
			tuplestore_putvalues(rsinfo->setResult, desc, datums, nulls);
			MemoryContextSwitchTo(old);
			MemoryContextReset(rowcxt);
		}
	}
	PG_CATCH();
	{
		/* an error of ours: the engine's handle freed, no database work */
		kb_call_abort(call);
		if (q != NULL)
			api->query_close(q);
		PG_RE_THROW();
	}
	PG_END_TRY();
	kb_trace_end(&tc, NULL, spec.query, 0, q, NULL);
	api->query_close(q);
	kb_call_finish(call);
	return (Datum) 0;
}

/* A query's whole result in a format, as text in call->out. */
static StringInfo
query_to_text(FunctionCallInfo fcinfo, KbQuerySpec *spec, const char *format, bool for_json)
{
	const KbApi *api = kb_api();
	KbError		err = {0};
	KbCall	   *call;
	KbQuery    *q = NULL;
	StringInfo	out = makeStringInfo();
	char	   *context = statement_context("SPARQL query", spec->query);
	int32		rc;
	KbTraceCall tc;

	(void) fcinfo;
	spec->explain = kb_trace_wants_plan();
	call = kb_call_begin(false, context);
	call->out = out;
	kb_trace_begin(call, &tc);
	rc = api->query_open(&call->host, spec, &q, &err);
	kb_call_check(call, rc, &err);
	if (for_json && api->query_kind(q) == KB_QUERY_CONSTRUCT)
		format = "application/ld+json";
	rc = api->query_write(q, format, &err);
	if (rc == KB_OK)
		kb_trace_end(&tc, NULL, spec->query, 0, q, NULL);
	/* after a host's error the callbacks of the close do nothing */
	api->query_close(q);
	kb_call_check(call, rc, &err);
	kb_call_finish(call);
	return out;
}

/*
 * kb.sparql_json(query text, base_iri text, default_graphs text[], named_graphs text[])
 * RETURNS jsonb
 */
Datum
kb_sparql_json(PG_FUNCTION_ARGS)
{
	KbQuerySpec spec;
	StringInfo	out;

	query_spec(fcinfo, &spec, 1, 2);
	out = query_to_text(fcinfo, &spec, "application/sparql-results+json", true);
	PG_RETURN_DATUM(DirectFunctionCall1(jsonb_in, CStringGetDatum(out->data)));
}

/*
 * kb.sparql_text(query text, format text, base_iri text, default_graphs text[],
 *				  named_graphs text[]) RETURNS text
 */
Datum
kb_sparql_text(PG_FUNCTION_ARGS)
{
	KbQuerySpec spec;
	StringInfo	out;
	char	   *format = opt_text(fcinfo, 1);

	query_spec(fcinfo, &spec, 2, 3);
	out = query_to_text(fcinfo, &spec, format ? format : "json", false);
	PG_RETURN_TEXT_P(cstring_to_text_with_len(out->data, out->len));
}

/* kb.sparql_explain(query text, base_iri text) RETURNS jsonb */
Datum
kb_sparql_explain(PG_FUNCTION_ARGS)
{
	const KbApi *api = kb_api();
	KbQuerySpec spec;
	KbError		err = {0};
	KbCall	   *call;
	KbQuery    *q = NULL;
	char	   *json = NULL;
	char	   *context;
	int32		rc;
	KbTraceCall tc;

	query_spec(fcinfo, &spec, 1, -1);
	spec.explain = true;
	context = statement_context("SPARQL query", spec.query);
	call = kb_call_begin(false, context);
	kb_trace_begin(call, &tc);
	rc = api->query_open(&call->host, &spec, &q, &err);
	kb_call_check(call, rc, &err);
	/* the statistics are the evaluation's: run it to its end */
	for (;;)
	{
		const KbTerm *values;
		bool		done;

		rc = api->query_next(q, &values, &done, &err);
		if (rc != KB_OK || done)
			break;
	}
	if (rc == KB_OK)
		rc = api->query_explain(q, &json, &err);
	if (rc == KB_OK)
		kb_trace_end(&tc, NULL, spec.query, 0, q, NULL);
	api->query_close(q);
	kb_call_check(call, rc, &err);
	kb_call_finish(call);
	PG_RETURN_DATUM(DirectFunctionCall1(jsonb_in, CStringGetDatum(kb_take_string(json))));
}

static bool
may_read_server_files(void)
{
	return has_privs_of_role(GetUserId(), ROLE_PG_READ_SERVER_FILES);
}

/*
 * kb.sparql_update(update text, base_iri text, using_graphs text[],
 *					using_named_graphs text[]) RETURNS void
 */
Datum
kb_sparql_update(PG_FUNCTION_ARGS)
{
	const KbApi *api = kb_api();
	KbUpdateSpec spec;
	KbError		err = {0};
	KbCall	   *call;
	char	   *update;
	char	   *context;
	int32		rc;
	KbTraceCall tc;

	PreventCommandIfReadOnly("kb.sparql_update()");
	if (PG_ARGISNULL(0))
		ereport(ERROR,
				(errcode(ERRCODE_NULL_VALUE_NOT_ALLOWED),
				 errmsg("the update is NULL")));
	update = text_to_cstring(PG_GETARG_TEXT_PP(0));
	memset(&spec, 0, sizeof(spec));
	spec.size = sizeof(spec);
	spec.update = kb_str(update);
	spec.base_iri = kb_str(opt_text(fcinfo, 1));
	spec.using_graphs = opt_text_array(fcinfo, 2, &spec.nusing_graphs);
	spec.using_named_graphs = opt_text_array(fcinfo, 3, &spec.nusing_named_graphs);
	spec.union_default_graph = kb_union_default_graph;
	spec.with_mappings = kb_with_mappings;
	spec.allow_load = may_read_server_files();
	context = statement_context("SPARQL update", spec.update);

	call = kb_call_begin(true, context);
	kb_trace_begin(call, &tc);
	rc = api->update(&call->host, &spec, &err);
	kb_call_check(call, rc, &err);
	kb_call_finish(call);
	kb_trace_end(&tc, "UPDATE", spec.update, 0, NULL, NULL);
	PG_RETURN_VOID();
}

/* A document into the store; a bulk load's quads staged, then put in once (store.c) */
static int64
load(KbLoadSpec *spec, bool bulk)
{
	const KbApi *api = kb_api();
	KbError		err = {0};
	KbBulk	   *staging = bulk ? kb_bulk_begin() : NULL;
	KbCall	   *call;
	int64		count = 0;
	int32		rc;
	KbTraceCall tc;

	call = kb_call_begin(true, "loading an RDF document");
	call->bulk = bulk;
	kb_trace_begin(call, &tc);
	rc = api->load(&call->host, spec, &count, &err);
	kb_call_check(call, rc, &err);
	kb_call_finish(call);
	if (staging != NULL)
		kb_bulk_finish(staging);
	if (tc.stmt != NULL)
	{
		kb_trace_count(tc.stmt, KB_TRACE_LOADED, count);
		if (bulk)
			kb_trace_count(tc.stmt, KB_TRACE_BULK, 1);
	}
	kb_trace_end(&tc, "LOAD", kb_str(NULL), 0, NULL, NULL);
	return count;
}

/*
 * kb.load(data text, format text, graph text, base_iri text, lenient boolean,
 *		   bulk boolean) RETURNS bigint
 */
Datum
kb_load(PG_FUNCTION_ARGS)
{
	KbLoadSpec	spec;
	text	   *data;

	PreventCommandIfReadOnly("kb.load()");
	if (PG_ARGISNULL(0))
		PG_RETURN_INT64(0);
	data = PG_GETARG_TEXT_PP(0);
	memset(&spec, 0, sizeof(spec));
	spec.size = sizeof(spec);
	spec.data = kb_str_len(VARDATA_ANY(data), VARSIZE_ANY_EXHDR(data));
	spec.format = kb_str(opt_text(fcinfo, 1) ? opt_text(fcinfo, 1) : "turtle");
	spec.graph = kb_str(opt_text(fcinfo, 2));
	spec.base_iri = kb_str(opt_text(fcinfo, 3));
	spec.lenient = PG_NARGS() > 4 && !PG_ARGISNULL(4) && PG_GETARG_BOOL(4);
	spec.batch = kb_load_batch;
	PG_RETURN_INT64(load(&spec, PG_NARGS() > 5 && !PG_ARGISNULL(5) && PG_GETARG_BOOL(5)));
}

/*
 * kb.load_file(path text, format text, graph text, base_iri text, lenient boolean,
 *				bulk boolean) RETURNS bigint
 */
Datum
kb_load_file(PG_FUNCTION_ARGS)
{
	KbLoadSpec	spec;
	char	   *path;

	PreventCommandIfReadOnly("kb.load_file()");
	if (!may_read_server_files())
		ereport(ERROR,
				(errcode(ERRCODE_INSUFFICIENT_PRIVILEGE),
				 errmsg("permission denied to read a server file"),
				 errdetail("Only roles with privileges of the \"%s\" role may load server files.",
						   "pg_read_server_files")));
	if (PG_ARGISNULL(0))
		ereport(ERROR,
				(errcode(ERRCODE_NULL_VALUE_NOT_ALLOWED),
				 errmsg("the path is NULL")));
	path = text_to_cstring(PG_GETARG_TEXT_PP(0));
	memset(&spec, 0, sizeof(spec));
	spec.size = sizeof(spec);
	spec.path = kb_str(path);
	spec.format = kb_str(opt_text(fcinfo, 1));
	spec.graph = kb_str(opt_text(fcinfo, 2));
	spec.base_iri = kb_str(opt_text(fcinfo, 3));
	spec.lenient = PG_NARGS() > 4 && !PG_ARGISNULL(4) && PG_GETARG_BOOL(4);
	spec.batch = kb_load_batch;
	PG_RETURN_INT64(load(&spec, PG_NARGS() > 5 && !PG_ARGISNULL(5) && PG_GETARG_BOOL(5)));
}

/* kb.dump(format text, graph text, with_mappings boolean) RETURNS text */
Datum
kb_dump(PG_FUNCTION_ARGS)
{
	const KbApi *api = kb_api();
	KbDumpSpec	spec;
	KbError		err = {0};
	KbCall	   *call;
	StringInfo	out = makeStringInfo();
	char	   *graph = opt_text(fcinfo, 1);
	int32		rc;

	memset(&spec, 0, sizeof(spec));
	spec.size = sizeof(spec);
	spec.format = kb_str(opt_text(fcinfo, 0) ? opt_text(fcinfo, 0) : "nquads");
	if (graph != NULL && strcmp(graph, "default") == 0)
		spec.default_graph = true;
	else
		spec.graph = kb_str(graph);
	spec.with_mappings = PG_NARGS() > 2 && !PG_ARGISNULL(2) && PG_GETARG_BOOL(2);
	call = kb_call_begin(false, "dumping the store");
	call->out = out;
	rc = api->dump(&call->host, &spec, &err);
	kb_call_check(call, rc, &err);
	kb_call_finish(call);
	PG_RETURN_TEXT_P(cstring_to_text_with_len(out->data, out->len));
}

/* kb.term(key uuid) RETURNS text: the term in N-Triples' syntax */
Datum
kb_term(PG_FUNCTION_ARGS)
{
	const KbApi *api = kb_api();
	pg_uuid_t  *uuid = PG_GETARG_UUID_P(0);
	KbKey		key;
	KbError		err = {0};
	KbCall	   *call;
	char	   *ntriples = NULL;
	int32		rc;

	memcpy(key.b, uuid->data, 16);
	call = kb_call_begin(false, NULL);
	rc = api->term_text(&call->host, &key, &ntriples, &err);
	kb_call_check(call, rc, &err);
	kb_call_finish(call);
	if (ntriples == NULL)
		PG_RETURN_NULL();
	PG_RETURN_TEXT_P(cstring_to_text(kb_take_string(ntriples)));
}

/* kb.term_key(term text) RETURNS uuid: the key of a term in N-Triples' syntax */
Datum
kb_term_key(PG_FUNCTION_ARGS)
{
	const KbApi *api = kb_api();
	char	   *ntriples = text_to_cstring(PG_GETARG_TEXT_PP(0));
	KbKey		key;
	KbTerm		term;
	bool		hashed;
	KbError		err = {0};
	pg_uuid_t  *uuid;
	int32		rc;

	rc = api->term_parse(ntriples, &key, &term, &hashed, &err);
	if (rc != KB_OK)
		kb_raise(&err, NULL);
	uuid = palloc(sizeof(pg_uuid_t));
	memcpy(uuid->data, key.b, 16);
	PG_RETURN_UUID_P(uuid);
}

/*
 * kb.check_mapping(name text, document text, base_iri text) RETURNS jsonb:
 * the mapping parsed and checked against the tables it names, as the
 * engine summarizes it; kb.add_mapping() stores it then.
 */
Datum
kb_check_mapping(PG_FUNCTION_ARGS)
{
	const KbApi *api = kb_api();
	KbMappingRow row;
	KbError		err = {0};
	KbCall	   *call;
	char	   *summary = NULL;
	int32		rc;

	if (PG_ARGISNULL(0) || PG_ARGISNULL(1))
		ereport(ERROR,
				(errcode(ERRCODE_NULL_VALUE_NOT_ALLOWED),
				 errmsg("a mapping's name and document are not NULL")));
	memset(&row, 0, sizeof(row));
	row.name = kb_str(opt_text(fcinfo, 0));
	row.document = kb_str(opt_text(fcinfo, 1));
	row.base_iri = kb_str(opt_text(fcinfo, 2));
	call = kb_call_begin(false, psprintf("R2RML mapping \"%s\"", row.name.ptr));
	rc = api->mapping_check(&call->host, &row, &summary, &err);
	kb_call_check(call, rc, &err);
	kb_call_finish(call);
	PG_RETURN_DATUM(DirectFunctionCall1(jsonb_in, CStringGetDatum(kb_take_string(summary))));
}

/*
 * kb.direct_mapping_json(tables jsonb, base_iri text) RETURNS text: the
 * W3C Direct Mapping of tables, described as kb.direct_mapping() gathers
 * them from the catalogs, as an R2RML document.
 */
Datum
kb_direct_mapping_json(PG_FUNCTION_ARGS)
{
	const KbApi *api = kb_api();
	char	   *tables = text_to_cstring(PG_GETARG_TEXT_PP(0));
	char	   *base = text_to_cstring(PG_GETARG_TEXT_PP(1));
	char	   *document = NULL;
	KbError		err = {0};
	int32		rc;

	rc = api->direct_mapping(tables, base, &document, &err);
	if (rc != KB_OK)
		kb_raise(&err, "generating a direct mapping");
	PG_RETURN_TEXT_P(cstring_to_text(kb_take_string(document)));
}

/* kb.engine_version() RETURNS text */
Datum
kb_engine_version(PG_FUNCTION_ARGS)
{
	(void) fcinfo;
	PG_RETURN_TEXT_P(cstring_to_text(kb_api()->version()));
}

/*
 * A trigger on kb.mappings: a change drops the backend's plans of the
 * mappings' SQL.  Other backends find a mapping's new revision when they
 * next read kb.mappings, and compile it anew.
 */
Datum
kb_mappings_changed_trigger(PG_FUNCTION_ARGS)
{
	if (!CALLED_AS_TRIGGER(fcinfo))
		elog(ERROR, "kb_mappings_changed_trigger: not called as a trigger");
	kb_mappings_changed();
	return PointerGetDatum(NULL);
}
