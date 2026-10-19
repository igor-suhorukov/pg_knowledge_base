/* SPDX-License-Identifier: Apache-2.0 */
/*-------------------------------------------------------------------------
 *
 * engine.c
 *	  Loading libkb_engine.so in a backend or worker on first use, never in
 *	  the postmaster, and raising its errors as PostgreSQL's
 *	  (docs/design.md §2.2).
 *
 *	  The engine runs on the calling thread and starts none of its own, so
 *	  loading it is a dlopen() and a check of its ABI version; it reaches
 *	  PostgreSQL only through the callbacks of a call (call.c).
 *
 *-------------------------------------------------------------------------
 */
#include "postgres.h"

#include <dlfcn.h>

#include "miscadmin.h"
#include "utils/elog.h"

#include "pg_knowledge_base.h"

static const KbApi *api = NULL;

const KbApi *
kb_api(void)
{
	char		path[MAXPGPATH];
	void	   *handle;
	kb_engine_api_fn entry;
	const KbApi *table;

	if (api != NULL)
		return api;

	if (!IsUnderPostmaster && IsPostmasterEnvironment)
		elog(ERROR, "the knowledge base engine library is never loaded in the postmaster");

	if (kb_engine_library == NULL || kb_engine_library[0] == '\0' ||
		strcmp(kb_engine_library, "$libdir/libkb_engine.so") == 0)
		snprintf(path, sizeof(path), "%s/libkb_engine.so", pkglib_path);
	else
		strlcpy(path, kb_engine_library, sizeof(path));

	handle = dlopen(path, RTLD_NOW | RTLD_LOCAL);
	if (handle == NULL)
		ereport(ERROR,
				(errcode(ERRCODE_UNDEFINED_FILE),
				 errmsg("could not load the knowledge base engine library \"%s\": %s",
						path, dlerror())));

	*(void **) &entry = dlsym(handle, "kb_engine_api");
	if (entry == NULL)
		ereport(ERROR,
				(errcode(ERRCODE_UNDEFINED_FUNCTION),
				 errmsg("the knowledge base engine library \"%s\" has no symbol kb_engine_api",
						path)));

	table = entry(KB_ABI_VERSION);
	if (table == NULL || table->abi_version != KB_ABI_VERSION || table->size < sizeof(KbApi))
		ereport(ERROR,
				(errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
				 errmsg("the knowledge base engine library \"%s\" was built for another ABI", path),
				 errdetail("The extension expects ABI version %d.", KB_ABI_VERSION)));
	api = table;
	return api;
}

bool
kb_engine_loaded(void)
{
	return api != NULL;
}

/* A string the library allocated, copied into palloc'd memory and freed */
char *
kb_take_string(char *s)
{
	char	   *copy;

	if (s == NULL)
		return NULL;
	copy = pstrdup(s);
	if (api != NULL)
		api->free_string(s);
	return copy;
}

int
kb_sqlstate(int32 code)
{
	switch (code)
	{
		case KB_ERR_SYNTAX:
			return ERRCODE_SYNTAX_ERROR;
		case KB_ERR_INVALID:
			return ERRCODE_INVALID_PARAMETER_VALUE;
		case KB_ERR_EVALUATION:
			return ERRCODE_DATA_EXCEPTION;
		case KB_ERR_UNSUPPORTED:
			return ERRCODE_FEATURE_NOT_SUPPORTED;
		case KB_ERR_NOT_FOUND:
			return ERRCODE_UNDEFINED_OBJECT;
		case KB_ERR_EXISTS:
			return ERRCODE_DUPLICATE_OBJECT;
		case KB_ERR_MAPPING:
			return ERRCODE_INVALID_OBJECT_DEFINITION;
		case KB_ERR_MEMORY:
			return ERRCODE_OUT_OF_MEMORY;
		default:
			return ERRCODE_INTERNAL_ERROR;
	}
}

/*
 * The engine's error, raised: its message, freed, as the error's.  A host's
 * error (KB_ERR_HOST) is never raised here: the call that kept it re-throws
 * it (kb_call_check()).
 */
void
kb_raise(KbError *err, const char *context)
{
	char	   *message = kb_take_string(err->message);
	int			code = err->code;

	err->message = NULL;
	ereport(ERROR,
			(errcode(kb_sqlstate(code)),
			 errmsg_internal("%s", message ? message : "the knowledge base engine failed"),
			 context ? errcontext("%s", context) : 0));
	pg_unreachable();
}
