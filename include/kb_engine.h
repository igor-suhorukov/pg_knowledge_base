/* SPDX-License-Identifier: Apache-2.0 */
/*-------------------------------------------------------------------------
 *
 * kb_engine.h
 *	  The C ABI of libkb_engine.so, the Rust library that links oxigraph's
 *	  SPARQL stack -- its parser, optimizer, evaluator, results formats and
 *	  RDF parsers -- (docs/design.md §2.2).  The extension loads the library
 *	  with dlopen() in a backend as it first needs it, never in the
 *	  postmaster, and reaches every entry point through the table
 *	  kb_engine_api() returns.
 *
 * Rules of the boundary:
 *
 *	- Every call is made on the backend's thread, and the engine runs on it:
 *	  oxigraph's evaluator is single-threaded, and the library starts no
 *	  thread of its own.
 *	- The engine reaches PostgreSQL only through the host's callbacks
 *	  (KbHost), and only while an entry point that was given them runs, or
 *	  while a handle made with them is open.  A callback never raises: the
 *	  extension catches PostgreSQL's error inside it, keeps it, and returns
 *	  KB_HOST_ERROR; the engine then returns from the entry point with
 *	  KB_ERR_HOST, unwinding only its own frames, and the extension re-throws
 *	  the error it kept.  So no longjmp crosses a Rust frame, and no Rust
 *	  unwinding crosses a C one: a panic inside the library becomes
 *	  KB_ERR_PANIC.
 *	- Cancels and terminations are PostgreSQL's errors like any other: the
 *	  callbacks run CHECK_FOR_INTERRUPTS(), and the engine calls
 *	  check_interrupts() in its own long loops.
 *	- Strings cross with their lengths and are not NUL-terminated unless
 *	  said.  What a callback returns lives until the next call of the same
 *	  callback on the same cursor or context; what the engine returns lives
 *	  until the next call on the same handle, or, where it is a char * the
 *	  caller owns, until free_string().
 *	- An RDF term is identified by a key of 16 bytes (KbKey), which the
 *	  engine computes from the term alone and the store keeps in uuid
 *	  columns: small values inline, others a 120-bit hash whose term the
 *	  dictionary (kb.terms) holds (docs/design.md §3.1).  The extension
 *	  never interprets a key, but for the default graph's: all zeros, the
 *	  smallest uuid.
 *
 *-------------------------------------------------------------------------
 */
#ifndef KB_ENGINE_H
#define KB_ENGINE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define KB_ABI_VERSION		2

/* Status codes of the entry points */
#define KB_OK					0
#define KB_ERR_INTERNAL			1	/* a fault of the library */
#define KB_ERR_SYNTAX			2	/* a SPARQL, RDF or R2RML text that does not parse */
#define KB_ERR_INVALID			3	/* the caller's input otherwise */
#define KB_ERR_EVALUATION		4	/* SPARQL's evaluation failed */
#define KB_ERR_HOST				5	/* a callback failed: the host's error is kept */
#define KB_ERR_UNSUPPORTED		6
#define KB_ERR_NOT_FOUND		7	/* a graph or mapping that is not there */
#define KB_ERR_EXISTS			8	/* a graph that is already there */
#define KB_ERR_MAPPING			9	/* a non-conforming R2RML mapping */
#define KB_ERR_PANIC			10
#define KB_ERR_MEMORY			11	/* past kb.engine_memory_limit */

/* What a callback returns */
#define KB_HOST_OK				0
#define KB_HOST_ERROR			1	/* PostgreSQL raised; the extension keeps the error */

typedef struct KbError
{
	int32_t		code;
	int32_t		http_status;	/* the status an HTTP response gives it, or 0 */
	char	   *message;		/* NUL-terminated; free_string() */
} KbError;

typedef struct KbKey
{
	uint8_t		b[16];
} KbKey;

typedef struct KbStr
{
	const char *ptr;
	int64_t		len;
} KbStr;

/* An RDF term, as text */
#define KB_TERM_NONE		0	/* unbound, or not found */
#define KB_TERM_IRI			1
#define KB_TERM_BNODE		2	/* value: the label, without "_:" */
#define KB_TERM_LITERAL		3	/* datatype: always set; lang: rdf:langString's */

typedef struct KbTerm
{
	int32_t		kind;
	KbStr		value;
	KbStr		datatype;
	KbStr		lang;
} KbTerm;

/* A dictionary row: a hashed key's term (kb.terms) */
typedef struct KbDictEntry
{
	bool		found;
	KbStr		value;
	KbStr		extra;			/* the datatype or language, or empty */
} KbDictEntry;

/* Which graphs a scan of the store reads, when it names none */
#define KB_GRAPHS_ONE		0	/* the graph g names (the default graph: zeros) */
#define KB_GRAPHS_NAMED		1	/* every named graph, and not the default graph */
#define KB_GRAPHS_ALL		2	/* every graph */

/* What a write does */
#define KB_WRITE_INSERT		1
#define KB_WRITE_DELETE		2

/* What graph_clear() empties */
#define KB_CLEAR_GRAPH		1	/* the graph g names, its quads; a named graph stays */
#define KB_CLEAR_DROP		2	/* the named graph g names, quads and name */
#define KB_CLEAR_NAMED		3	/* every named graph's quads; the names stay */
#define KB_CLEAR_DROP_NAMED 4	/* every named graph, quads and names */
#define KB_CLEAR_ALL		5	/* every quad and every name */

/* A SQL value's class, for R2RML's natural RDF literals (R2RML §10.2) */
#define KB_SQL_STRING		0
#define KB_SQL_INTEGER		1
#define KB_SQL_DECIMAL		2
#define KB_SQL_FLOAT		3
#define KB_SQL_BOOLEAN		4
#define KB_SQL_DATE			5
#define KB_SQL_TIME			6
#define KB_SQL_TIMESTAMP	7
#define KB_SQL_BINARY		8	/* its text is hexadecimal, upper case */

/*
 * Options of sql_open(): KB_SQL_GENERIC_PLAN for a query run again and again
 * with other parameters -- a pattern's read for each solution of a join --
 * which PostgreSQL then plans once, for any parameters, where by default it
 * plans each run for its own (plan_cache_mode); KB_SQL_HASH_JOINS for a
 * query whose joins PostgreSQL cannot estimate -- the UNION ALLs of patterns
 * joined on keys the SQL makes -- planned without nested loops, so that a
 * join reads each side once (enable_nestloop)
 */
#define KB_SQL_GENERIC_PLAN	1
#define KB_SQL_HASH_JOINS	2

typedef struct KbColumn
{
	KbStr		name;
	uint32_t	type_oid;
	int32_t		sql_class;		/* KB_SQL_* */
	KbStr		type_name;		/* format_type()'s, for a cast */
} KbColumn;

/* A value of a row from sql_next(): its lexical form under R2RML, or NULL */
typedef struct KbSqlValue
{
	bool		isnull;
	KbStr		text;
} KbSqlValue;

/* A mapping, from kb.mappings */
typedef struct KbMappingRow
{
	KbStr		name;
	KbStr		document;
	KbStr		base_iri;		/* empty: none */
	int64_t		revision;
} KbMappingRow;

typedef struct KbCursor KbCursor;	/* the extension's, opaque to the engine */

/*
 * The host: PostgreSQL, as the extension gives it to one call.  ctx is
 * passed back to every callback that takes it.  A callback that is NULL is
 * not offered to this call; the engine refuses what needs it.
 */
typedef struct KbHost
{
	uint32_t	size;			/* sizeof(KbHost) */
	void	   *ctx;

	/*
	 * The store (src/store.c).  quads_open(): s, p, o, g NULL where unbound;
	 * graphs says which graphs an unbound g reads.  quads_next() fills up to
	 * max quads, four keys each in the order s, p, o, g, and sets *n; 0 at
	 * the end.
	 */
	int32_t		(*quads_open) (void *ctx, const KbKey *s, const KbKey *p, const KbKey *o,
							   const KbKey *g, int32_t graphs, KbCursor **cursor);
	int32_t		(*quads_next) (KbCursor *cursor, KbKey *quads, int32_t max, int32_t *n);
	void		(*quads_close) (KbCursor *cursor);
	/* The dictionary's rows of n keys */
	int32_t		(*terms_get) (void *ctx, const KbKey *keys, int32_t n, KbDictEntry *entries);
	/* The named graphs kb.graphs holds, empty ones too: keys through quads_next(), g alone */
	int32_t		(*graphs_open) (void *ctx, KbCursor **cursor);
	int32_t		(*graph_exists) (void *ctx, const KbKey *g, bool *exists);
	/* Writes, in the caller's transaction: quads four keys each, s, p, o, g */
	int32_t		(*quads_write) (void *ctx, int32_t op, const KbKey *quads, int32_t n);
	int32_t		(*terms_write) (void *ctx, const KbKey *keys, const KbDictEntry *entries, int32_t n);
	int32_t		(*graphs_write) (void *ctx, int32_t op, const KbKey *keys, int32_t n);
	int32_t		(*graph_clear) (void *ctx, int32_t what, const KbKey *g);

	/*
	 * SQL, for R2RML (src/sql.c): a query's result columns; a query run
	 * with text parameters, its rows a batch at a time, ncolumns values
	 * each.
	 */
	int32_t		(*sql_describe) (void *ctx, const char *sql, int32_t *ncolumns, const KbColumn **columns);
	int32_t		(*sql_open) (void *ctx, const char *sql, int32_t nparams, const KbStr *params,
							 int32_t options, KbCursor **cursor, int32_t *ncolumns);
	int32_t		(*sql_next) (KbCursor *cursor, int32_t max, const KbSqlValue **values, int32_t *nrows);
	void		(*sql_close) (KbCursor *cursor);
	/* The rows PostgreSQL's plan of a query with its parameters expects: planned, not run */
	int32_t		(*sql_estimate) (void *ctx, const char *sql, int32_t nparams, const KbStr *params,
								 double *rows);
	/* The enabled mappings: rows valid until the call ends */
	int32_t		(*mappings_get) (void *ctx, int32_t *n, const KbMappingRow **rows);

	/* Output: serialized bytes, as they come */
	int32_t		(*write) (void *ctx, const char *data, size_t len);
	/* An HTTP response's head, before its first write() (the endpoint only) */
	int32_t		(*respond) (void *ctx, int32_t status, const char *content_type, const char *headers);

	int32_t		(*check_interrupts) (void *ctx);
} KbHost;

/*
 * The engine's settings, given before each call (configure()): the memory
 * its allocations may hold, over which a call fails at its next callback,
 * row or quad; the entries of its cache of the dictionary's terms; and the
 * host's flag of a pending interrupt, which the engine reads at each row it
 * makes and each quad it reads, and calls check_interrupts() when it is
 * set.
 */
/* A call's figures for a trace (KbApi's query_trace(), http_trace()) */
typedef struct KbTrace
{
	uint32_t	size;			/* sizeof(KbTrace) */
	char		operation[16];	/* SELECT, ASK, CONSTRUCT, DESCRIBE, UPDATE; "" */
	int64_t		rows;			/* rows, booleans or triples given */
	int32_t		plan_nodes;
	char	   *plan;			/* or NULL; free_string() */
} KbTrace;

typedef struct KbConfig
{
	uint32_t	size;			/* sizeof(KbConfig) */
	int64_t		memory_limit;	/* bytes; 0: none */
	int32_t		term_cache_entries;
	const volatile int32_t *interrupt_pending;	/* or NULL */
	bool		r2rml_pushdown;	/* basic graph patterns over mappings as one SQL query */
	bool		statistics;		/* the optimizer's estimates of the store and the mappings */
} KbConfig;

/* Opaque handles */
typedef struct KbQuery KbQuery;
typedef struct KbHttp KbHttp;

/* What a query is, after query_open() */
#define KB_QUERY_SELECT		1
#define KB_QUERY_ASK		2
#define KB_QUERY_CONSTRUCT	3	/* and DESCRIBE: triples */

typedef struct KbQuerySpec
{
	uint32_t	size;			/* sizeof(KbQuerySpec) */
	KbStr		query;
	KbStr		base_iri;		/* empty: none */
	int32_t		ndefault_graphs;	/* default-graph-uri: the default graph's graphs */
	const KbStr *default_graphs;
	int32_t		nnamed_graphs;	/* named-graph-uri: the named graphs */
	const KbStr *named_graphs;
	bool		union_default_graph;	/* the default graph is every graph's union */
	bool		with_mappings;	/* R2RML's virtual graphs beside the store */
	bool		explain;		/* keep the plan with statistics, for query_explain() */
} KbQuerySpec;

typedef struct KbUpdateSpec
{
	uint32_t	size;			/* sizeof(KbUpdateSpec) */
	KbStr		update;
	KbStr		base_iri;
	int32_t		nusing_graphs;	/* using-graph-uri */
	const KbStr *using_graphs;
	int32_t		nusing_named_graphs;	/* using-named-graph-uri */
	const KbStr *using_named_graphs;
	bool		union_default_graph;
	bool		with_mappings;	/* the WHERE clauses read R2RML's graphs too */
	bool		allow_load;		/* LOAD <file:...> may read the server's files */
} KbUpdateSpec;

typedef struct KbLoadSpec
{
	uint32_t	size;			/* sizeof(KbLoadSpec) */
	KbStr		data;			/* the document, or empty with path */
	KbStr		path;			/* a server file, or empty */
	KbStr		format;			/* a name, an extension or a media type */
	KbStr		base_iri;
	KbStr		graph;			/* the triples' graph: empty, the default graph */
	bool		lenient;		/* oxigraph's lenient parsing */
	int32_t		batch;			/* quads a write */
} KbLoadSpec;

typedef struct KbDumpSpec
{
	uint32_t	size;			/* sizeof(KbDumpSpec) */
	KbStr		format;
	KbStr		graph;			/* empty: every graph, the format then a dataset's */
	bool		default_graph;	/* the default graph alone */
	bool		with_mappings;	/* R2RML's virtual triples too */
} KbDumpSpec;

/* An HTTP request to the endpoint, as the session read it */
typedef struct KbHttpRequest
{
	uint32_t	size;			/* sizeof(KbHttpRequest) */
	KbStr		method;
	KbStr		path;			/* the target's path, the database's segment taken off */
	KbStr		query;			/* the target's query string, without "?" */
	KbStr		accept;
	KbStr		content_type;
	KbStr		base_iri;		/* scheme://host/path of the request, for relative IRIs */
	KbStr		body;
	bool		read_only;		/* kb.endpoint_read_only: updates refused */
	bool		union_default_graph;
	bool		with_mappings;
	int64_t		max_body;
} KbHttpRequest;

/* What a request asks, from http_prepare() */
#define KB_HTTP_ERROR		0	/* answer error.http_status with error.message */
#define KB_HTTP_READ		1	/* a query, or a read of the graph store */
#define KB_HTTP_WRITE		2	/* an update, or a write of the graph store */
#define KB_HTTP_STATIC		3	/* a page or a service description: no transaction */

typedef struct KbHttpInfo
{
	int32_t		kind;
	KbStr		activity;		/* for pg_stat_activity: the query or update */
} KbHttpInfo;

/* The entry points (engine/src/lib.rs) */
typedef struct KbApi
{
	uint32_t	size;			/* sizeof(KbApi) */
	uint32_t	abi_version;	/* KB_ABI_VERSION */
	const char *(*version) (void);
	void		(*free_string) (char *s);
	void		(*configure) (const KbConfig *config);
	int64_t		(*memory_used) (void);

	/*
	 * A query, parsed, optimized and evaluated lazily: its kind and
	 * variables, then either its solutions a row at a time (query_next(),
	 * values an array of nvariables terms, or triples s, p, o), or its whole
	 * result serialized through host->write (query_write(), format a name or
	 * a media type), and query_close() in any case.
	 */
	int32_t		(*query_open) (const KbHost *host, const KbQuerySpec *spec, KbQuery **query, KbError *err);
	int32_t		(*query_kind) (KbQuery *query);
	int32_t		(*query_variables) (KbQuery *query, int32_t *n, const KbStr **names);
	int32_t		(*query_next) (KbQuery *query, const KbTerm **values, bool *done, KbError *err);
	int32_t		(*query_write) (KbQuery *query, const char *format, KbError *err);
	int32_t		(*query_explain) (KbQuery *query, char **json, KbError *err);
	void		(*query_close) (KbQuery *query);

	/* A SPARQL update, its writes through the host's */
	int32_t		(*update) (const KbHost *host, const KbUpdateSpec *spec, KbError *err);
	/* An RDF document into the store; *count: the quads read */
	int32_t		(*load) (const KbHost *host, const KbLoadSpec *spec, int64_t *count, KbError *err);
	/* The store, or one graph, serialized through host->write */
	int32_t		(*dump) (const KbHost *host, const KbDumpSpec *spec, KbError *err);

	/* A key's term, in N-Triples' syntax; NULL when the dictionary has no row */
	int32_t		(*term_text) (const KbHost *host, const KbKey *key, char **text, KbError *err);
	/* A term in N-Triples' syntax: its key and, when hashed, its dictionary row */
	int32_t		(*term_parse) (const char *text, KbKey *key, KbTerm *term, bool *hashed, KbError *err);

	/*
	 * R2RML (engine/src/r2rml): a mapping document checked against the
	 * tables its logical tables name, *summary a JSON description; and the
	 * W3C Direct Mapping of tables described in JSON, as an R2RML document.
	 */
	int32_t		(*mapping_check) (const KbHost *host, const KbMappingRow *mapping, char **summary,
								  KbError *err);
	int32_t		(*direct_mapping) (const char *tables_json, const char *base_iri, char **document,
								   KbError *err);

	/* The SPARQL 1.1 protocol and graph store protocol (engine/src/http.rs) */
	int32_t		(*http_prepare) (const KbHttpRequest *request, KbHttp **http, KbHttpInfo *info,
								 KbError *err);
	int32_t		(*http_execute) (const KbHost *host, KbHttp *http, KbError *err);
	void		(*http_close) (KbHttp *http);

	/*
	 * Traces (src/trace.c): a query's figures -- its operation, its rows so
	 * far, and, opened to be explained, its plan, each node with its results
	 * and time, in at most max_bytes and, unless raw, its literals replaced
	 * by '?'; an endpoint request's, after http_execute(), its query having
	 * been evaluated with statistics where http_explain() asked; a SPARQL
	 * text, cut and normalized as a plan is; and the most the engine's
	 * memory held since the last reset.
	 */
	int32_t		(*query_trace) (KbQuery *query, int32_t max_bytes, bool raw, KbTrace *trace, KbError *err);
	void		(*http_explain) (KbHttp *http, bool on);
	int32_t		(*http_trace) (KbHttp *http, int32_t max_bytes, bool raw, KbTrace *trace, KbError *err);
	int32_t		(*text_trace) (KbStr text, int32_t max_bytes, bool raw, char **out, KbError *err);
	int64_t		(*memory_peak) (bool reset);
} KbApi;

typedef const KbApi *(*kb_engine_api_fn) (uint32_t abi_version);

#endif							/* KB_ENGINE_H */
