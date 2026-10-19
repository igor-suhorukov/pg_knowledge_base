/* SPDX-License-Identifier: Apache-2.0 */
/*-------------------------------------------------------------------------
 *
 * module.c
 *	  _PG_init: pg_knowledge_base's settings, and the SPARQL endpoint's
 *	  shared table and acceptor when the module is preloaded
 *	  (docs/design.md §2.1, §5).
 *
 * The SQL functions need no preloading: a backend loads the module, and
 * the engine library, on its first call.  The endpoint does: its acceptor
 * is a background worker the postmaster starts, and its sessions count in
 * a table of shared memory.
 *
 *-------------------------------------------------------------------------
 */
#include "postgres.h"

#include <limits.h>

#include "fmgr.h"
#include "miscadmin.h"
#include "utils/guc.h"

#include "pg_knowledge_base.h"
#include "endpoint/endpoint.h"

PG_MODULE_MAGIC_EXT(
					.name = "pg_knowledge_base",
					.version = KB_VERSION
);

char	   *kb_engine_library = NULL;
bool		kb_union_default_graph = false;
bool		kb_with_mappings = true;
int			kb_load_batch = 8192;
int			kb_engine_memory_limit = 1024 * 1024;
int			kb_term_cache_entries = 65536;

static void
define_settings(void)
{
	DefineCustomStringVariable("kb.engine_library",
							   "The knowledge base engine library.",
							   NULL,
							   &kb_engine_library,
							   "$libdir/libkb_engine.so",
							   PGC_SUSET, 0,
							   NULL, NULL, NULL);
	DefineCustomBoolVariable("kb.union_default_graph",
							 "SPARQL's default graph is the union of every graph.",
							 "Off, the default: the store's default graph alone, as oxigraph's.",
							 &kb_union_default_graph,
							 false,
							 PGC_USERSET, 0,
							 NULL, NULL, NULL);
	DefineCustomBoolVariable("kb.with_mappings",
							 "SPARQL reads R2RML's virtual graphs beside the store.",
							 NULL,
							 &kb_with_mappings,
							 true,
							 PGC_USERSET, 0,
							 NULL, NULL, NULL);
	DefineCustomIntVariable("kb.load_batch",
							"Quads a write of a load or an update carries.",
							NULL,
							&kb_load_batch,
							8192, 64, 1048576,
							PGC_USERSET, 0,
							NULL, NULL, NULL);
	DefineCustomIntVariable("kb.engine_memory_limit",
							"The memory the knowledge base engine may hold in a backend.",
							"What the engine allocates -- a query's joins, sorts, groups -- is "
							"not in PostgreSQL's memory contexts and work_mem does not bound it; "
							"past this a query fails.  0: no limit.",
							&kb_engine_memory_limit,
							1024 * 1024, 0, INT_MAX,
							PGC_SUSET, GUC_UNIT_KB,
							NULL, NULL, NULL);
	DefineCustomIntVariable("kb.term_cache_size",
							"Terms of the dictionary a backend keeps in its cache.",
							NULL,
							&kb_term_cache_entries,
							65536, 1024, INT_MAX / 2,
							PGC_USERSET, 0,
							NULL, NULL, NULL);
	kb_endpoint_define_settings();
	kb_trace_define_settings();
}

void
_PG_init(void)
{
	define_settings();
	MarkGUCPrefixReserved("kb");
	if (process_shared_preload_libraries_in_progress)
	{
		kb_endpoint_register();
		kb_trace_init();
	}
}
