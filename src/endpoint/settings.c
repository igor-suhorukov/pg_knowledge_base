/* SPDX-License-Identifier: Apache-2.0 */
/*-------------------------------------------------------------------------
 *
 * settings.c
 *	  The SPARQL endpoint's settings, its shared table of sessions, and its
 *	  acceptor's registration (docs/design.md §5.1).
 *
 * The acceptor runs whenever the module is preloaded, and listens on
 * kb.endpoint_listen_addresses, which a reload sets again: an empty list --
 * the default, as vexec_flight.listen_addresses's -- listens on none, and
 * the endpoint is off.  A reload that empties the list switches a running
 * endpoint off, and one that names addresses switches it on (acceptor.c,
 * session.c).
 *
 *-------------------------------------------------------------------------
 */
#include "postgres.h"

#include <limits.h>

#include "miscadmin.h"
#include "postmaster/bgworker.h"
#include "storage/ipc.h"
#include "storage/lwlock.h"
#include "storage/shmem.h"
#include "utils/guc.h"
#include "utils/varlena.h"

#include "endpoint/endpoint.h"

char	   *kb_endpoint_listen_addresses = NULL;
int			kb_endpoint_port = 7878;
char	   *kb_endpoint_ssl_cert_file = NULL;
char	   *kb_endpoint_ssl_key_file = NULL;
int			kb_endpoint_max_sessions = 3;
int			kb_endpoint_idle_session_timeout = 60 * 1000;
char	   *kb_endpoint_anonymous_role = NULL;
bool		kb_endpoint_read_only = false;
int			kb_endpoint_max_body = 128 * 1024;
bool		kb_endpoint_cors = false;

EndpointShared *kb_endpoint_shared = NULL;

static shmem_request_hook_type prev_shmem_request = NULL;
static shmem_startup_hook_type prev_shmem_startup = NULL;

static Size
shared_size(void)
{
	return add_size(offsetof(EndpointShared, slots),
					mul_size(sizeof(EndpointSlot), kb_endpoint_max_sessions));
}

static void
endpoint_shmem_request(void)
{
	if (prev_shmem_request)
		prev_shmem_request();
	RequestAddinShmemSpace(shared_size());
}

static void
endpoint_shmem_startup(void)
{
	bool		found;

	if (prev_shmem_startup)
		prev_shmem_startup();
	LWLockAcquire(AddinShmemInitLock, LW_EXCLUSIVE);
	kb_endpoint_shared = ShmemInitStruct("pg_knowledge_base endpoint sessions", shared_size(), &found);
	if (!found)
	{
		memset(kb_endpoint_shared, 0, shared_size());
		SpinLockInit(&kb_endpoint_shared->mutex);
		kb_endpoint_shared->nslots = kb_endpoint_max_sessions;
	}
	LWLockRelease(AddinShmemInitLock);
}

/*
 * The endpoint's sessions of a role or a database, logged in, for their
 * connection limits: PostgreSQL counts backends only.
 */
int
kb_endpoint_count_sessions(Oid roleid, Oid dboid)
{
	int			n = 0;
	int			i;

	if (kb_endpoint_shared == NULL)
		return 0;
	SpinLockAcquire(&kb_endpoint_shared->mutex);
	for (i = 0; i < kb_endpoint_shared->nslots; i++)
	{
		EndpointSlot *s = &kb_endpoint_shared->slots[i];

		if (s->state != ENDPOINT_SLOT_ACTIVE)
			continue;
		if ((OidIsValid(roleid) && s->roleid == roleid) ||
			(OidIsValid(dboid) && s->dboid == dboid))
			n++;
	}
	SpinLockRelease(&kb_endpoint_shared->mutex);
	return n;
}

/* kb.endpoint_listen_addresses: a list the acceptor can split */
static bool
check_listen_addresses(char **newval, void **extra, GucSource source)
{
	char	   *raw = pstrdup(*newval);
	List	   *elems;
	bool		ok = SplitGUCList(raw, ',', &elems);

	(void) extra;
	(void) source;
	list_free(elems);
	pfree(raw);
	if (!ok)
		GUC_check_errdetail("List syntax is invalid.");
	return ok;
}

/* Whether the endpoint is on: its list of addresses not empty. */
bool
kb_endpoint_on(void)
{
	return kb_endpoint_listen_addresses != NULL && kb_endpoint_listen_addresses[0] != '\0';
}

void
kb_endpoint_define_settings(void)
{
	DefineCustomStringVariable("kb.endpoint_listen_addresses",
							   "The addresses the SPARQL endpoint listens on.",
							   "A comma-separated list of host names or numeric addresses, "
							   "or * for all; empty, the default, listens on none: the endpoint "
							   "is off.  A reload switches the endpoint on and off.",
							   &kb_endpoint_listen_addresses,
							   "",
							   PGC_SIGHUP, GUC_LIST_INPUT,
							   check_listen_addresses, NULL, NULL);
	DefineCustomStringVariable("kb.endpoint_ssl_cert_file",
							   "The SPARQL endpoint's TLS certificate; empty takes ssl_cert_file's.",
							   "Without a certificate and a key the endpoint serves plain HTTP only, "
							   "which hostssl lines of pg_hba.conf do not match.",
							   &kb_endpoint_ssl_cert_file,
							   "",
							   PGC_SIGHUP, 0,
							   NULL, NULL, NULL);
	DefineCustomStringVariable("kb.endpoint_ssl_key_file",
							   "The SPARQL endpoint's TLS key; empty takes ssl_key_file's.",
							   NULL,
							   &kb_endpoint_ssl_key_file,
							   "",
							   PGC_SIGHUP, 0,
							   NULL, NULL, NULL);
	DefineCustomIntVariable("kb.endpoint_idle_session_timeout",
							"A SPARQL endpoint connection idle this long between requests closes.",
							"0 never closes one.",
							&kb_endpoint_idle_session_timeout,
							60 * 1000, 0, INT_MAX,
							PGC_SIGHUP, GUC_UNIT_MS,
							NULL, NULL, NULL);
	DefineCustomStringVariable("kb.endpoint_anonymous_role",
							   "The role a request without credentials logs in as.",
							   "Empty, the default: every request carries HTTP's basic credentials. "
							   "pg_hba.conf still decides: a password method answers 401.",
							   &kb_endpoint_anonymous_role,
							   "",
							   PGC_SIGHUP, 0,
							   NULL, NULL, NULL);
	DefineCustomBoolVariable("kb.endpoint_read_only",
							 "The endpoint refuses updates and graph store writes.",
							 NULL,
							 &kb_endpoint_read_only,
							 false,
							 PGC_SIGHUP, 0,
							 NULL, NULL, NULL);
	DefineCustomIntVariable("kb.endpoint_max_body",
							"The largest request body the endpoint reads.",
							NULL,
							&kb_endpoint_max_body,
							128 * 1024, 64, 1024 * 1024,
							PGC_SIGHUP, GUC_UNIT_KB,
							NULL, NULL, NULL);
	DefineCustomBoolVariable("kb.endpoint_cors",
							 "The endpoint answers browsers of any origin (CORS).",
							 NULL,
							 &kb_endpoint_cors,
							 false,
							 PGC_SIGHUP, 0,
							 NULL, NULL, NULL);

	/*
	 * The postmaster's settings, defined only while the postmaster loads
	 * shared_preload_libraries: later, init_custom_variable() exits with
	 * FATAL.  A backend that loads the module itself -- at CREATE EXTENSION,
	 * or at its first call -- has no endpoint, and leaves them undefined,
	 * as pg_prewarm leaves pg_prewarm.autoprewarm.
	 */
	if (!process_shared_preload_libraries_in_progress)
		return;
	DefineCustomIntVariable("kb.endpoint_port",
							"The SPARQL endpoint's TCP port.",
							NULL,
							&kb_endpoint_port,
							7878, 1, 65535,
							PGC_POSTMASTER, 0,
							NULL, NULL, NULL);
	DefineCustomIntVariable("kb.endpoint_max_sessions",
							"SPARQL endpoint sessions -- client connections -- at once.",
							"Each is a background worker, counted in max_worker_processes with "
							"the acceptor: 3, the default, fit PostgreSQL's default of 8.",
							&kb_endpoint_max_sessions,
							3, 1, MAX_BACKENDS,
							PGC_POSTMASTER, 0,
							NULL, NULL, NULL);
}

void
kb_endpoint_register(void)
{
	BackgroundWorker worker;

	prev_shmem_request = shmem_request_hook;
	shmem_request_hook = endpoint_shmem_request;
	prev_shmem_startup = shmem_startup_hook;
	shmem_startup_hook = endpoint_shmem_startup;

	/*
	 * The acceptor, whatever the list: an empty one it listens on none of,
	 * until a reload names addresses
	 */
	memset(&worker, 0, sizeof(worker));
	worker.bgw_flags = BGWORKER_SHMEM_ACCESS | BGWORKER_BACKEND_DATABASE_CONNECTION;
	worker.bgw_start_time = BgWorkerStart_RecoveryFinished;
	worker.bgw_restart_time = 5;
	strlcpy(worker.bgw_library_name, "pg_knowledge_base", BGW_MAXLEN);
	strlcpy(worker.bgw_function_name, "kb_endpoint_acceptor_main", BGW_MAXLEN);
	strlcpy(worker.bgw_name, "pg_knowledge_base endpoint acceptor", BGW_MAXLEN);
	strlcpy(worker.bgw_type, "pg_knowledge_base acceptor", BGW_MAXLEN);
	RegisterBackgroundWorker(&worker);
}
