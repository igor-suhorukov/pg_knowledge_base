/* SPDX-License-Identifier: Apache-2.0 */
/*-------------------------------------------------------------------------
 *
 * session.c
 *	  A SPARQL endpoint session: one dynamic background worker with one
 *	  thread, which owns its client's socket as a backend does
 *	  (docs/design.md §5.2) -- vexec_flight's session
 *	  (pg_accel/modules/vexec_flight/src/session.c), HTTP/1.1 in place of
 *	  HTTP/2 and gRPC.
 *
 * The session takes its client's socket from the acceptor, does TLS when
 * the client starts it, and serves the connection's requests one at a
 * time, as a backend serves one statement at a time.
 *
 * Logging in.  The first request carries the credentials: HTTP's basic
 * authentication, "Authorization: Basic ...", or none, when
 * kb.endpoint_anonymous_role names a role that pg_hba.conf trusts.  The
 * database is the path's first segment: the paths of oxigraph's server are
 * below a database's name -- /<database>/query, /<database>/update,
 * /<database>/sparql, /<database>/store -- and any other path is answered
 * 404 before a login (answer_path()).  The acceptor checks the login
 * (login.c) -- pg_hba.conf with the client's address and TLS, the
 * password, the role's and the database's limits -- and the session then
 * connects to that role and database
 * (BackgroundWorkerInitializeConnectionByOid, PG19:src/include/postmaster/
 * bgworker.h:157), with MyProcPort describing its client, so that
 * pg_stat_activity shows the client's address and pg_stat_ssl its TLS.  A
 * refused login is answered -- 401 with a challenge where credentials may
 * help -- and the connection stays open for the client to try again.  A
 * later request of the connection must name the same database and the
 * same user: a background worker connects to one database once, and a
 * request for another is answered 421, Misdirected Request, the
 * connection closed, so that the client asks it on a new one.
 *
 * Requests.  What a request asks and its answer are the engine's
 * (engine/src/http.rs, oxigraph's server's): http_prepare() reads it and
 * says whether it reads or writes, and the session runs it in a
 * transaction of its own, read only for a query or a read of the graph
 * store, as an implicit transaction does: so permissions, row-level
 * security on R2RML's tables, settings -- a role's and a database's too,
 * statement_timeout among them -- and pg_stat_activity behave as for a
 * backend's statement.  The transaction commits before the response ends:
 * an update the client sees answered is committed.
 *
 * Errors.  An error in a request is the request's: the loop catches it as
 * PostgresMain does (postgres.c, its sigsetjmp block), aborts the
 * transaction, and answers the error's HTTP status, its message, and its
 * SQLSTATE in an X-SQLSTATE header -- or, when the response's head has gone
 * out, cuts the connection before the response's end.  A FATAL ends the
 * session; the request under way is answered 503 first, when it can be.
 *
 * Cancels.  pg_cancel_backend() and pg_terminate_backend() signal the
 * session as any backend.  A cancel ends the request under way; between
 * requests it means nothing.
 *
 * Switched off.  A reload that empties kb.endpoint_listen_addresses
 * switches the endpoint off (acceptor.c), and no request starts after it:
 * the session reads the reload between requests -- one that comes while a
 * request is served waits for the request's end, so that the request ends
 * as it would have -- and then closes its connection, at once when it is
 * idle (http.c).  A connection the acceptor took before the switch, whose
 * session starts after it, has its first request answered 503.
 *
 *-------------------------------------------------------------------------
 */
#include "postgres.h"

#include <execinfo.h>
#include <openssl/evp.h>
#include <signal.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include "access/xact.h"
#include "catalog/namespace.h"
#include "common/base64.h"
#include "common/ip.h"
#include "common/sha2.h"
#include "jit/jit.h"
#include "libpq/libpq-be.h"
#include "miscadmin.h"
#include "pgstat.h"
#include "postmaster/bgworker.h"
#include "postmaster/interrupt.h"
#include "postmaster/postmaster.h"
#include "replication/slot.h"
#include "storage/ipc.h"
#include "storage/latch.h"
#include "storage/proc.h"
#include "tcop/tcopprot.h"
#include "utils/guc.h"
#include "utils/memutils.h"
#include "utils/portal.h"
#include "utils/ps_status.h"
#include "utils/snapmgr.h"
#include "utils/timeout.h"
#include "utils/timestamp.h"
#include "utils/wait_event.h"

#include "executor/spi.h"
#include "pg_knowledge_base.h"
#include "pg_otel_probe_api.h"
#include "endpoint/endpoint.h"

typedef struct EndpointSession
{
	HttpConn   *conn;
	KbTls	   *tls;
	int			sock;
	int			slot;
	bool		logged_in;
	bool		anonymous;		/* logged in without credentials */
	uint8		credentials[32];	/* SHA-256 of the login's "user:password" */
	char		user[NAMEDATALEN];
	char		database[NAMEDATALEN];
	MemoryContext reqcxt;		/* a request's, reset after it */
	HttpRequest *req;			/* the request being served */
	HttpResponse *resp;			/* its response */
	KbHttp	   *http;			/* the engine's handle of it */
	int32		kind;			/* KB_HTTP_READ, _WRITE, _STATIC */
	KbStr		activity;		/* its query or update, for traces */
	char		route[16];		/* /query, /update, /sparql, /store, / */
	bool		in_statement;	/* run by kb.endpoint_request() */
	bool		xact_started;
	int			error_status;	/* an error's status, when its raiser named one */
} EndpointSession;

static EndpointSession *session = NULL;
static int	acceptor_link = -1;

/* ---------------------------------------------------------------------
 * The slot, and the acceptor's link
 * ---------------------------------------------------------------------
 */

static void
release_slot(int code, Datum arg)
{
	int			slot = DatumGetInt32(arg);

	(void) code;
	SpinLockAcquire(&kb_endpoint_shared->mutex);
	if (kb_endpoint_shared->slots[slot].pid == MyProcPid)
		memset(&kb_endpoint_shared->slots[slot], 0, sizeof(EndpointSlot));
	SpinLockRelease(&kb_endpoint_shared->mutex);
}

/* Wait, with the latch, for a socket's events: false when time ran out. */
static bool
wait_socket(int fd, int events, int timeout_ms)
{
	int			rc = WaitLatchOrSocket(MyLatch, WL_LATCH_SET | WL_EXIT_ON_PM_DEATH | WL_TIMEOUT | events,
									   fd, timeout_ms, WAIT_EVENT_CLIENT_READ);

	if (rc & WL_LATCH_SET)
	{
		ResetLatch(MyLatch);
		CHECK_FOR_INTERRUPTS();
	}
	return (rc & events) != 0;
}

/* All of len bytes from a socket, within timeout_ms. */
static bool
read_full(int fd, void *buf, size_t len, int timeout_ms)
{
	size_t		got = 0;
	TimestampTz deadline = TimestampTzPlusMilliseconds(GetCurrentTimestamp(), timeout_ms);

	while (got < len)
	{
		ssize_t		n = recv(fd, (char *) buf + got, len - got, MSG_DONTWAIT);

		if (n > 0)
		{
			got += n;
			continue;
		}
		if (n == 0 || (errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR))
			return false;
		if (TimestampDifferenceMilliseconds(GetCurrentTimestamp(), deadline) <= 0)
			return false;
		(void) wait_socket(fd, WL_SOCKET_READABLE,
						   (int) TimestampDifferenceMilliseconds(GetCurrentTimestamp(), deadline));
	}
	return true;
}

/*
 * The client's socket, from the acceptor: connect to its UNIX-domain
 * socket, show the slot's token, take the socket.  The link stays open for
 * the login.
 */
static int
take_client_socket(int slot, uint64 token, int *link_out)
{
	struct sockaddr_un addr;
	EndpointHello hello;
	int			link;
	struct msghdr msg;
	struct iovec iov;
	char		ok;
	union
	{
		struct cmsghdr hdr;
		char		buf[CMSG_SPACE(sizeof(int))];
	}			control;
	struct cmsghdr *cmsg;
	int			fd = -1;
	TimestampTz deadline;

	memset(&addr, 0, sizeof(addr));
	addr.sun_family = AF_UNIX;
	snprintf(addr.sun_path, sizeof(addr.sun_path), ENDPOINT_SOCKET_NAME, kb_endpoint_port);
	link = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
	if (link < 0 || connect(link, (struct sockaddr *) &addr, sizeof(addr)) < 0)
		ereport(FATAL,
				(errcode_for_socket_access(),
				 errmsg("pg_knowledge_base session could not reach the acceptor at \"%s\": %m",
						addr.sun_path)));
	hello.magic = ENDPOINT_MAGIC;
	hello.slot = slot;
	hello.token = token;
	if (send(link, &hello, sizeof(hello), MSG_NOSIGNAL) != sizeof(hello))
		ereport(FATAL, (errmsg("pg_knowledge_base session could not greet the acceptor: %m")));

	deadline = TimestampTzPlusMilliseconds(GetCurrentTimestamp(), 10000);
	for (;;)
	{
		ssize_t		n;
		long		remaining;

		memset(&msg, 0, sizeof(msg));
		iov.iov_base = &ok;
		iov.iov_len = 1;
		msg.msg_iov = &iov;
		msg.msg_iovlen = 1;
		msg.msg_control = control.buf;
		msg.msg_controllen = sizeof(control.buf);
		n = recvmsg(link, &msg, MSG_DONTWAIT | MSG_CMSG_CLOEXEC);
		if (n == 1)
			break;
		if (n == 0 || (errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR))
			ereport(FATAL, (errmsg("pg_knowledge_base session: the acceptor closed its link")));
		remaining = TimestampDifferenceMilliseconds(GetCurrentTimestamp(), deadline);
		if (remaining <= 0)
			ereport(FATAL, (errmsg("pg_knowledge_base session: the acceptor did not hand over its client")));
		(void) wait_socket(link, WL_SOCKET_READABLE, (int) remaining);
	}
	for (cmsg = CMSG_FIRSTHDR(&msg); cmsg; cmsg = CMSG_NXTHDR(&msg, cmsg))
		if (cmsg->cmsg_level == SOL_SOCKET && cmsg->cmsg_type == SCM_RIGHTS)
			memcpy(&fd, CMSG_DATA(cmsg), sizeof(int));
	if (fd < 0)
		ereport(FATAL, (errmsg("pg_knowledge_base session: the acceptor sent no socket")));
	*link_out = link;
	return fd;
}

/* ---------------------------------------------------------------------
 * Errors as HTTP's statuses
 * ---------------------------------------------------------------------
 */

static int
http_status_for(int sqlerrcode)
{
	switch (sqlerrcode)
	{
		case ERRCODE_INSUFFICIENT_PRIVILEGE:
			return 403;
		case ERRCODE_UNDEFINED_TABLE:
			return 404;
		case ERRCODE_QUERY_CANCELED:
		case ERRCODE_ADMIN_SHUTDOWN:
		case ERRCODE_CRASH_SHUTDOWN:
		case ERRCODE_CANNOT_CONNECT_NOW:
			return 503;
		case ERRCODE_T_R_SERIALIZATION_FAILURE:
		case ERRCODE_T_R_DEADLOCK_DETECTED:
			return 409;
		case ERRCODE_READ_ONLY_SQL_TRANSACTION:
			return 403;
		case ERRCODE_PROGRAM_LIMIT_EXCEEDED:
			return 413;
	}
	switch (ERRCODE_TO_CATEGORY(sqlerrcode))
	{
		case ERRCODE_TO_CATEGORY(ERRCODE_DATA_EXCEPTION):
		case ERRCODE_TO_CATEGORY(ERRCODE_SYNTAX_ERROR_OR_ACCESS_RULE_VIOLATION):
			return 400;
		case ERRCODE_TO_CATEGORY(ERRCODE_INTEGRITY_CONSTRAINT_VIOLATION):
		case ERRCODE_TO_CATEGORY(ERRCODE_TRANSACTION_ROLLBACK):
			return 409;
		case ERRCODE_TO_CATEGORY(ERRCODE_INVALID_AUTHORIZATION_SPECIFICATION):
			return 401;
		case ERRCODE_TO_CATEGORY(ERRCODE_INSUFFICIENT_RESOURCES):
		case ERRCODE_TO_CATEGORY(ERRCODE_CONNECTION_EXCEPTION):
		case ERRCODE_TO_CATEGORY(ERRCODE_OPERATOR_INTERVENTION):
			return 503;
	}
	return 500;
}

/* An error's text for the client: its message, detail and hint. */
static char *
error_text(ErrorData *edata)
{
	StringInfoData s;

	initStringInfo(&s);
	appendStringInfoString(&s, edata->message ? edata->message : "an error");
	if (edata->detail)
		appendStringInfo(&s, "\nDETAIL: %s", edata->detail);
	if (edata->hint)
		appendStringInfo(&s, "\nHINT: %s", edata->hint);
	return s.data;
}

static const char *
cors_headers(void)
{
	return kb_endpoint_cors ? "Access-Control-Allow-Origin: *\r\n" : "";
}

/* ---------------------------------------------------------------------
 * Logging in
 * ---------------------------------------------------------------------
 */

/* The user and password of "Authorization: Basic <base64>", or false. */
static bool
basic_credentials(const char *header, char *user, char *password)
{
	char	   *b64;
	char	   *decoded;
	int			len;
	char	   *colon;

	if (header == NULL || pg_strncasecmp(header, "Basic ", 6) != 0)
		return false;
	/* padded to a multiple of 4: some clients send base64 without its "=" */
	b64 = psprintf("%s%.*s", header + 6, (int) ((4 - strlen(header + 6) % 4) % 4), "===");
	decoded = palloc(pg_b64_dec_len(strlen(b64)) + 1);
	len = pg_b64_decode(b64, strlen(b64), (uint8 *) decoded, pg_b64_dec_len(strlen(b64)));
	if (len < 0)
		return false;
	decoded[len] = '\0';
	colon = strchr(decoded, ':');
	if (colon == NULL || colon - decoded >= NAMEDATALEN || colon == decoded ||
		strlen(colon + 1) >= ENDPOINT_MAX_PASSWORD || memchr(decoded, '\0', len) != NULL)
		return false;
	memcpy(user, decoded, colon - decoded);
	user[colon - decoded] = '\0';
	strlcpy(password, colon + 1, ENDPOINT_MAX_PASSWORD);
	explicit_bzero(decoded, len);
	return true;
}

/*
 * SHA-256 of "user:password": a later request of the connection must carry
 * the credentials its login did, not only the same user's name, since a
 * proxy may carry several clients' requests on one connection.
 */
static void
credentials_hash(const char *user, const char *password, uint8 *out)
{
	char	   *both = psprintf("%s:%s", user, password);
	unsigned int len = 0;

	/* OpenSSL's one-shot digest: PostgreSQL's would want a resource owner */
	if (EVP_Digest(both, strlen(both), out, &len, EVP_sha256(), NULL) != 1 ||
		len != PG_SHA256_DIGEST_LENGTH)
		elog(ERROR, "could not hash the credentials");
	explicit_bzero(both, strlen(both));
	pfree(both);
}

/* A Port describing the client, for hba's lines and pg_stat_activity. */
static Port *
client_port(void)
{
	Port	   *port = MemoryContextAllocZero(TopMemoryContext, sizeof(Port));
	char		host[NI_MAXHOST];
	char		service[NI_MAXSERV];

	port->sock = session->sock;
	port->raddr.salen = sizeof(port->raddr.addr);
	(void) getpeername(port->sock, (struct sockaddr *) &port->raddr.addr, &port->raddr.salen);
	port->laddr.salen = sizeof(port->laddr.addr);
	(void) getsockname(port->sock, (struct sockaddr *) &port->laddr.addr, &port->laddr.salen);
	host[0] = service[0] = '\0';
	(void) pg_getnameinfo_all(&port->raddr.addr, port->raddr.salen, host, sizeof(host),
							  service, sizeof(service), NI_NUMERICHOST | NI_NUMERICSERV);
	port->remote_host = MemoryContextStrdup(TopMemoryContext, host);
	port->remote_port = MemoryContextStrdup(TopMemoryContext, service);
	port->user_name = session->user;
	port->database_name = session->database;
	if (session->tls)
	{
		/* pg_stat_ssl reads the connection's TLS through it */
		port->ssl_in_use = true;
		port->ssl = kb_tls_ssl(session->tls);
	}
	return port;
}

/*
 * Log in as user to database: the acceptor checks the login, and the
 * session connects.  False, the request answered, when it is refused.
 */
static bool
log_in(const HttpRequest *req, const char *user, const char *password, bool has_password,
	   const char *database)
{
	EndpointLoginRequest login;
	EndpointLoginReply reply;
	Port	   *port;
	const char *appname = http_header(req, "x-application-name");

	memset(&login, 0, sizeof(login));
	login.magic = ENDPOINT_MAGIC;
	login.slot = session->slot;
	strlcpy(login.user, user, sizeof(login.user));
	strlcpy(login.database, database, sizeof(login.database));
	if (has_password)
		strlcpy(login.password, password, sizeof(login.password));
	login.has_password = has_password;
	login.ssl = session->tls != NULL;
	{
		socklen_t	len = sizeof(login.addr);

		(void) getpeername(session->sock, (struct sockaddr *) &login.addr, &len);
		login.addrlen = len;
	}
	if (send(acceptor_link, &login, sizeof(login), MSG_NOSIGNAL) != sizeof(login) ||
		!read_full(acceptor_link, &reply, sizeof(reply), 60000) ||
		reply.magic != ENDPOINT_MAGIC)
		ereport(FATAL, (errmsg("pg_knowledge_base session lost its link to the acceptor")));
	explicit_bzero(login.password, sizeof(login.password));
	if (!reply.ok)
	{
		char	   *headers;

		reply.message[sizeof(reply.message) - 1] = '\0';
		reply.sqlstate[5] = '\0';
		headers = psprintf("%sX-SQLSTATE: %s\r\n%s",
						   reply.challenge ?
						   "WWW-Authenticate: Basic realm=\"pg_knowledge_base\", charset=\"UTF-8\"\r\n" : "",
						   reply.sqlstate, cors_headers());
		if (!http_send_simple(session->conn, req, reply.http_status, headers, reply.message,
							  req->keep_alive))
			http_conn_close(session->conn);
		return false;
	}

	strlcpy(session->user, user, NAMEDATALEN);
	strlcpy(session->database, database, NAMEDATALEN);
	session->anonymous = !has_password;
	credentials_hash(user, has_password ? password : "", session->credentials);
	port = client_port();
	MyProcPort = port;
	SpinLockAcquire(&kb_endpoint_shared->mutex);
	kb_endpoint_shared->slots[session->slot].roleid = reply.roleid;
	kb_endpoint_shared->slots[session->slot].dboid = reply.dboid;
	kb_endpoint_shared->slots[session->slot].state = ENDPOINT_SLOT_ACTIVE;
	SpinLockRelease(&kb_endpoint_shared->mutex);
	close(acceptor_link);
	acceptor_link = -1;

	BackgroundWorkerInitializeConnectionByOid(reply.dboid, reply.roleid, 0);
	SetConfigOption("application_name", appname ? appname : "pg_knowledge_base endpoint",
					PGC_USERSET, PGC_S_CLIENT);
	pgstat_report_activity(STATE_IDLE, NULL);
	set_ps_display(psprintf("%s %s %s", user, database, port->remote_host));
	session->logged_in = true;
	return true;
}

/* ---------------------------------------------------------------------
 * A request
 * ---------------------------------------------------------------------
 */

static bool
session_write(void *arg, const char *data, size_t len)
{
	(void) arg;
	return http_response_write(session->resp, data, len);
}

static int32
session_respond(void *arg, int32 status, const char *content_type, const char *headers)
{
	(void) arg;
	http_response_head(session->resp, status, content_type,
					   psprintf("%s%s", headers ? headers : "", cors_headers()));
	return KB_HOST_OK;
}

static int
hexval(char c)
{
	if (c >= '0' && c <= '9')
		return c - '0';
	if (c >= 'a' && c <= 'f')
		return c - 'a' + 10;
	if (c >= 'A' && c <= 'F')
		return c - 'A' + 10;
	return -1;
}

static char *
percent_decode(const char *s, size_t len)
{
	char	   *out = palloc(len + 1);
	size_t		i;
	size_t		j = 0;

	for (i = 0; i < len; i++)
	{
		/* %00 stays as it is: no name holds a NUL */
		if (s[i] == '%' && i + 2 < len && hexval(s[i + 1]) >= 0 && hexval(s[i + 2]) >= 0 &&
			(hexval(s[i + 1]) | hexval(s[i + 2])) != 0)
		{
			out[j++] = (char) (hexval(s[i + 1]) * 16 + hexval(s[i + 2]));
			i += 2;
		}
		else
			out[j++] = s[i];
	}
	out[j] = '\0';
	return out;
}

/*
 * The request's database, its path's first segment, percent-decoded, and the
 * path the engine answers, the rest.
 */
static char *
split_path(const char *path, const char **engine_path)
{
	const char *segment = path + 1;
	const char *slash = strchr(segment, '/');
	size_t		len = slash ? (size_t) (slash - segment) : strlen(segment);

	*engine_path = slash ? slash : "/";
	return percent_decode(segment, len);
}

/* The paths of oxigraph's server, which the engine answers below a database's */
static bool
is_endpoint_path(const char *engine_path)
{
	return strcmp(engine_path, "/") == 0 ||
		strcmp(engine_path, "/query") == 0 ||
		strcmp(engine_path, "/update") == 0 ||
		strcmp(engine_path, "/sparql") == 0 ||
		strcmp(engine_path, "/store") == 0 ||
		strncmp(engine_path, "/store/", 7) == 0;
}

/*
 * A request whose path is not a database's path of the engine's, answered
 * before any login: true when it was.  A database's page is /<database>/,
 * where its relative links are the database's own paths: /<database>
 * without the slash is sent there.
 */
static bool
answer_path(HttpRequest *req, const char *database, const char *engine_path)
{
	int			status;
	const char *headers = cors_headers();
	const char *message = NULL;

	if (database[0] == '\0')
	{
		status = 404;
		message = "the endpoint's paths are a database's: /<database>/query, /<database>/update, "
			"/<database>/sparql, /<database>/store";
	}
	else if (strlen(database) >= NAMEDATALEN)
	{
		status = 400;
		message = psprintf("a database's name is shorter than %d bytes", NAMEDATALEN);
	}
	else if (!is_endpoint_path(engine_path))
	{
		status = 404;
		message = psprintf("%s %s is not supported by this server", req->method, req->path);
	}
	else if (strchr(req->path + 1, '/') == NULL &&
			 (strcmp(req->method, "GET") == 0 || strcmp(req->method, "HEAD") == 0))
	{
		status = 308;
		headers = psprintf("Location: %s/%s%s\r\n%s", req->path,
						   req->query[0] ? "?" : "", req->query, cors_headers());
	}
	else
		return false;
	if (!http_send_simple(session->conn, req, status, headers, message, req->keep_alive))
		http_conn_close(session->conn);
	return true;
}

/* The request's URL, without its query: the base of relative IRIs. */
static char *
request_base(const HttpRequest *req)
{
	const char *proto = http_header(req, "x-forwarded-proto");
	const char *host = http_header(req, "x-forwarded-host");

	if (host == NULL)
		host = http_header(req, "host");
	if (host == NULL)
		host = psprintf("localhost:%d", kb_endpoint_port);
	if (proto == NULL)
		proto = session->tls ? "https" : "http";
	return psprintf("%s://%s%s", proto, host, req->path);
}

/*
 * The caller's trace context, where pg_otel_probe is loaded (its PLAN.md
 * §4.7.4), as vexec_flight takes it from a call's gRPC metadata: a request's
 * W3C traceparent and tracestate headers, as OpenTelemetry's HTTP
 * instrumentations send them, make the statement of the request a child of
 * the caller's span, for the request's transaction alone.  A malformed one
 * is ignored.
 */
static void
caller_trace_context(void)
{
	const PgOtelProbeContextApi *otel = pg_otel_probe_context_api();
	const char *traceparent;

	if (otel == NULL || session->req == NULL)
		return;
	traceparent = http_header(session->req, "traceparent");
	if (traceparent != NULL)
		(void) otel->set_parent(traceparent, http_header(session->req, "tracestate"), true);
}

/*
 * The request's transaction begun, as postgres.c's start_xact_command()
 * begins one; the request's memory stays current, so that what its response
 * holds outlives the commit.
 */
static void
start_xact(bool read_only)
{
	if (!session->xact_started)
	{
		MemoryContext old = CurrentMemoryContext;

		StartTransactionCommand();
		MemoryContextSwitchTo(old);
		session->xact_started = true;
		if (read_only)
			XactReadOnly = true;
		caller_trace_context();
	}
	if (StatementTimeout > 0 && (StatementTimeout < TransactionTimeout || TransactionTimeout == 0))
	{
		if (!get_timeout_active(STATEMENT_TIMEOUT))
			enable_timeout_after(STATEMENT_TIMEOUT, StatementTimeout);
	}
	else if (get_timeout_active(STATEMENT_TIMEOUT))
		disable_timeout(STATEMENT_TIMEOUT, false);
	PushActiveSnapshot(GetTransactionSnapshot());
}

static void
finish_xact(void)
{
	if (get_timeout_active(STATEMENT_TIMEOUT))
		disable_timeout(STATEMENT_TIMEOUT, false);
	if (session->xact_started)
	{
		PopActiveSnapshot();
		CommitTransactionCommand();
		session->xact_started = false;
		MemoryContextSwitchTo(session->reqcxt);
	}
}

/* An engine path's first segment: /query, /update, /sparql, /store, or / */
static void
route_of(const char *engine_path, char *route, size_t size)
{
	const char *slash = strchr(engine_path + 1, '/');
	size_t		len = slash ? (size_t) (slash - engine_path) : strlen(engine_path);

	strlcpy(route, engine_path, Min(len + 1, size));
}

/*
 * The request run by the engine, its response written as the engine makes
 * it; traced, with its method, route and status, its query's or update's
 * text, rows and plan on its statement's span.
 */
static void
run_request(void)
{
	const KbApi *api = kb_api();
	KbError		err = {0};
	KbCall	   *call;
	KbTraceCall tc;
	int32		rc;

	call = kb_call_begin(session->kind == KB_HTTP_WRITE, NULL);
	call->write_fn = session_write;
	call->respond_fn = session_respond;
	call->write_arg = session;
	kb_trace_begin(call, &tc);
	if (tc.stmt != NULL)
		api->http_explain(session->http, kb_trace_wants_plan());
	rc = api->http_execute(&call->host, session->http, &err);
	if (rc != KB_OK && !call->failed)
		session->error_status = err.http_status;
	kb_call_check(call, rc, &err);
	kb_call_finish(call);
	if (tc.stmt != NULL)
	{
		kb_trace_http(tc.stmt, session->req->method, session->route,
					  http_response_status(session->resp));
		kb_trace_end(&tc, NULL, session->activity, 0, NULL, session->http);
	}
}

/*
 * With pg_otel_probe loaded, a request runs as a statement of its own,
 * SELECT kb.endpoint_request(), which the probe traces as it traces a
 * backend's: a span of its own, a child of the caller's span where the
 * request named one, whose children are the SQL statements the request
 * runs -- R2RML's queries, the store's writes -- and on which trace.c puts
 * the request's SPARQL.
 */
static void
run_as_statement(void)
{
	static SPIPlanPtr plan = NULL;
	int			rc;

	if (SPI_connect() != SPI_OK_CONNECT)
		elog(ERROR, "SPI_connect failed");
	if (plan == NULL)
	{
		plan = SPI_prepare("SELECT " KB_SCHEMA ".endpoint_request()", 0, NULL);
		if (plan == NULL || SPI_keepplan(plan) != 0)
			elog(ERROR, "the endpoint's statement does not prepare: %s",
				 SPI_result_code_string(SPI_result));
	}
	session->in_statement = true;
	rc = SPI_execute_plan(plan, NULL, NULL, false, 0);
	session->in_statement = false;
	if (rc != SPI_OK_SELECT)
		elog(ERROR, "the endpoint's statement failed: %s", SPI_result_code_string(rc));
	SPI_finish();
}

PG_FUNCTION_INFO_V1(kb_endpoint_request);

/* kb.endpoint_request() RETURNS void: the request a session's statement runs */
Datum
kb_endpoint_request(PG_FUNCTION_ARGS)
{
	(void) fcinfo;
	if (session == NULL || !session->in_statement || session->http == NULL)
		ereport(ERROR,
				(errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
				 errmsg("kb.endpoint_request() runs the SPARQL endpoint's requests, in its sessions only")));
	session->in_statement = false;
	run_request();
	PG_RETURN_VOID();
}

static void
serve(HttpRequest *req)
{
	const KbApi *api;
	const char *engine_path;
	char	   *database = split_path(req->path, &engine_path);
	char		user[NAMEDATALEN];
	char		password[ENDPOINT_MAX_PASSWORD];
	bool		has_credentials;
	KbHttpRequest kreq;
	KbHttpInfo	info;
	KbError		err = {0};
	int32		rc;
	const char *auth = http_header(req, "authorization");

	/* the request is a statement: pg_stat_activity's query_start, a span's */
	SetCurrentStatementStartTimestamp();
	session->in_statement = false;

	/* a browser's preflight carries no credentials */
	if (strcmp(req->method, "OPTIONS") == 0)
	{
		if (!http_send_simple(session->conn, req, kb_endpoint_cors ? 204 : 405,
							  kb_endpoint_cors ?
							  "Access-Control-Allow-Origin: *\r\n"
							  "Access-Control-Allow-Methods: GET, HEAD, POST, PUT, DELETE, OPTIONS\r\n"
							  "Access-Control-Allow-Headers: Authorization, Content-Type, Accept\r\n"
							  "Access-Control-Max-Age: 600\r\n" :
							  "Allow: GET, HEAD, POST, PUT, DELETE\r\n",
							  NULL, req->keep_alive))
			http_conn_close(session->conn);
		return;
	}

	if (answer_path(req, database, engine_path))
		return;

	has_credentials = basic_credentials(auth, user, password);
	if (!has_credentials)
	{
		password[0] = '\0';
		if (auth != NULL || kb_endpoint_anonymous_role == NULL || kb_endpoint_anonymous_role[0] == '\0')
		{
			if (!http_send_simple(session->conn, req, 401,
								  psprintf("WWW-Authenticate: Basic realm=\"pg_knowledge_base\", charset=\"UTF-8\"\r\n%s",
										   cors_headers()),
								  auth ? "log in with HTTP's basic authentication" :
								  "credentials are required: log in with HTTP's basic authentication",
								  req->keep_alive))
				http_conn_close(session->conn);
			return;
		}
		strlcpy(user, kb_endpoint_anonymous_role, sizeof(user));
	}

	if (!session->logged_in)
	{
		bool		ok = log_in(req, user, password, has_credentials, database);

		explicit_bzero(password, sizeof(password));
		if (!ok)
			return;
		/* connecting ran transactions of its own */
		MemoryContextSwitchTo(session->reqcxt);
	}
	else
	{
		uint8		hash[PG_SHA256_DIGEST_LENGTH];
		bool		same;

		credentials_hash(user, password, hash);
		explicit_bzero(password, sizeof(password));
		same = strcmp(database, session->database) == 0 && strcmp(user, session->user) == 0 &&
			has_credentials != session->anonymous &&
			timingsafe_bcmp(hash, session->credentials, PG_SHA256_DIGEST_LENGTH) == 0;
		if (!same)
		{
			(void) http_send_simple(session->conn, req, 421,
									psprintf("X-SQLSTATE: 08004\r\n%s", cors_headers()),
									psprintf("this connection serves user \"%s\" and database \"%s\": open another",
											 session->user, session->database),
									false);
			http_conn_close(session->conn);
			return;
		}
	}

	api = kb_api();
	memset(&kreq, 0, sizeof(kreq));
	kreq.size = sizeof(kreq);
	kreq.method = kb_str(req->method);
	kreq.path = kb_str(engine_path);
	kreq.query = kb_str(req->query);
	kreq.accept = kb_str(http_header(req, "accept") ? http_header(req, "accept") : "");
	kreq.content_type = kb_str(http_header(req, "content-type") ? http_header(req, "content-type") : "");
	kreq.base_iri = kb_str(request_base(req));
	kreq.body = kb_str_len(req->body, req->body_len);
	kreq.read_only = kb_endpoint_read_only;
	kreq.union_default_graph = kb_union_default_graph;
	kreq.with_mappings = kb_with_mappings;
	kreq.max_body = (int64) kb_endpoint_max_body * 1024;
	rc = api->http_prepare(&kreq, &session->http, &info, &err);
	if (rc != KB_OK)
	{
		char	   *message = kb_take_string(err.message);

		if (!http_send_simple(session->conn, req, err.http_status ? err.http_status : 400,
							  cors_headers(), message, req->keep_alive))
			http_conn_close(session->conn);
		return;
	}

	session->resp = http_response_create(session->conn, req);
	if (info.activity.len > 0)
	{
		char	   *activity = kb_cstring(info.activity);

		debug_query_string = activity;
		pgstat_report_activity(STATE_RUNNING, activity);
	}
	set_ps_display(req->method);
	session->kind = info.kind;
	session->activity = info.activity;
	route_of(engine_path, session->route, sizeof(session->route));
	start_xact(info.kind != KB_HTTP_WRITE);
	if (kb_trace_probe_loaded() && OidIsValid(get_namespace_oid(KB_SCHEMA, true)))
		run_as_statement();
	else
		run_request();
	api->http_close(session->http);
	session->http = NULL;
	finish_xact();
	debug_query_string = NULL;
	if (!http_response_end(session->resp))
		http_conn_close(session->conn);
	session->resp = NULL;
}

/* ---------------------------------------------------------------------
 * The session's end
 * ---------------------------------------------------------------------
 */

static void
session_exit(int code, Datum arg)
{
	(void) code;
	(void) arg;
	if (session == NULL || session->conn == NULL || !http_conn_alive(session->conn))
		return;
	/*
	 * A FATAL -- a terminate, the postmaster's shutdown -- answers the request
	 * under way 503, when its response has not begun.
	 */
	if (session->req != NULL && (session->resp == NULL || !http_response_started(session->resp)))
		(void) http_send_simple(session->conn, session->req, 503, "X-SQLSTATE: 57P01\r\n",
								"terminating connection due to administrator command", false);
	http_conn_close(session->conn);
	session->conn = NULL;
}

/* TLS or plain HTTP, by the client's first byte: 0x16 begins TLS's handshake. */
static bool
start_tls(int sock)
{
	char		first;
	TimestampTz deadline = TimestampTzPlusMilliseconds(GetCurrentTimestamp(),
													   AuthenticationTimeout * 1000);

	for (;;)
	{
		ssize_t		n = recv(sock, &first, 1, MSG_PEEK | MSG_DONTWAIT);
		long		remaining;

		if (n == 1)
			break;
		if (n == 0 || (errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR))
			proc_exit(0);
		remaining = TimestampDifferenceMilliseconds(GetCurrentTimestamp(), deadline);
		if (remaining <= 0)
			proc_exit(0);
		(void) wait_socket(sock, WL_SOCKET_READABLE, (int) remaining);
	}
	if (first != 0x16)
		return true;
	session->tls = kb_tls_create();
	if (session->tls == NULL)
	{
		ereport(COMMERROR,
				(errcode(ERRCODE_PROTOCOL_VIOLATION),
				 errmsg("pg_knowledge_base endpoint: a client began TLS, and the endpoint has no certificate")));
		return false;
	}
	return kb_tls_accept(session->tls, sock, AuthenticationTimeout * 1000);
}

/* KB_DEBUG_BACKTRACE in the server's environment: a crash's frames to stderr */
static void
crash_backtrace(int signo)
{
	void	   *frames[64];
	int			n = backtrace(frames, lengthof(frames));

	backtrace_symbols_fd(frames, n, 2);
	signal(signo, SIG_DFL);
	raise(signo);
}

void
kb_endpoint_session_main(Datum arg)
{
	sigjmp_buf	local_sigjmp_buf;
	int			slot = DatumGetInt32(arg);
	uint64		token;
	EndpointSession *s;
	volatile int served = 0;	/* the connection's requests begun: kept across an error */

	pqsignal(SIGHUP, SignalHandlerForConfigReload);
	/* a terminate is served at CHECK_FOR_INTERRUPTS(), as a backend's */
	pqsignal(SIGTERM, die);
	if (getenv("KB_DEBUG_BACKTRACE") != NULL)
		signal(SIGSEGV, crash_backtrace);
	BackgroundWorkerUnblockSignals();

	memcpy(&token, MyBgworkerEntry->bgw_extra, sizeof(token));
	s = MemoryContextAllocZero(TopMemoryContext, sizeof(EndpointSession));
	session = s;
	s->slot = slot;
	s->reqcxt = AllocSetContextCreate(TopMemoryContext, "pg_knowledge_base request",
									  ALLOCSET_DEFAULT_SIZES);

	/* the slot: the acceptor reserved it with this token */
	SpinLockAcquire(&kb_endpoint_shared->mutex);
	if (slot < 0 || slot >= kb_endpoint_shared->nslots ||
		kb_endpoint_shared->slots[slot].state != ENDPOINT_SLOT_RESERVED ||
		kb_endpoint_shared->slots[slot].token != token)
	{
		SpinLockRelease(&kb_endpoint_shared->mutex);
		ereport(FATAL, (errmsg("pg_knowledge_base session: slot %d is not reserved for it", slot)));
	}
	kb_endpoint_shared->slots[slot].state = ENDPOINT_SLOT_ATTACHED;
	kb_endpoint_shared->slots[slot].pid = MyProcPid;
	SpinLockRelease(&kb_endpoint_shared->mutex);
	before_shmem_exit(release_slot, Int32GetDatum(slot));

	s->sock = take_client_socket(slot, token, &acceptor_link);
	if (!start_tls(s->sock))
		proc_exit(0);
	s->conn = http_conn_create(s->sock, s->tls);
	before_shmem_exit(session_exit, 0);

	if (sigsetjmp(local_sigjmp_buf, 1) != 0)
	{
		ErrorData  *edata;
		int			status;

		error_context_stack = NULL;
		HOLD_INTERRUPTS();
		disable_all_timeouts(false);
		QueryCancelPending = false;
		MemoryContextSwitchTo(TopMemoryContext);
		edata = CopyErrorData();
		status = s->error_status > 0 ? s->error_status : http_status_for(edata->sqlerrcode);
		s->error_status = 0;
		EmitErrorReport();
		debug_query_string = NULL;
		if (s->logged_in)
		{
			AbortCurrentTransaction();
			PortalErrorCleanup();
			if (MyReplicationSlot != NULL)
				ReplicationSlotRelease();
			ReplicationSlotCleanup(false);
			jit_reset_after_error();
		}
		s->xact_started = false;
		if (s->http)
		{
			kb_api()->http_close(s->http);
			s->http = NULL;
		}
		FlushErrorState();
		if (s->req != NULL && http_conn_alive(s->conn))
		{
			if (s->resp == NULL || !http_response_started(s->resp))
			{
				if (!http_send_simple(s->conn, s->req, status,
									  psprintf("X-SQLSTATE: %s\r\n%s",
											   unpack_sql_state(edata->sqlerrcode), cors_headers()),
									  error_text(edata), s->req->keep_alive))
					http_conn_close(s->conn);
			}
			else
			{
				/* the head is out: the response cannot end well, nor the connection go on */
				http_conn_close(s->conn);
			}
			if (s->req && !s->req->keep_alive)
				http_conn_close(s->conn);
		}
		s->req = NULL;
		s->resp = NULL;
		FreeErrorData(edata);
		MemoryContextReset(s->reqcxt);
		if (s->logged_in)
			pgstat_report_activity(STATE_IDLE, NULL);
		RESUME_INTERRUPTS();
	}
	PG_exception_stack = &local_sigjmp_buf;

	for (;;)
	{
		HttpRequest *req;
		HttpRead	r;
		int			status;
		const char *message;
		int			idle;
		bool		keep_alive;

		if (!http_conn_alive(s->conn))
			break;
		/* a reload that came while a request was served */
		if (ConfigReloadPending)
		{
			ConfigReloadPending = false;
			ProcessConfigFile(PGC_SIGHUP);
		}
		/* the endpoint switched off: a connection that has had a request closes */
		if (served > 0 && !kb_endpoint_on())
			break;
		MemoryContextSwitchTo(s->reqcxt);
		req = palloc0(sizeof(HttpRequest));
		idle = s->logged_in ? kb_endpoint_idle_session_timeout : AuthenticationTimeout * 1000;
		r = http_read_request(s->conn, req, idle, Max(AuthenticationTimeout, 60) * 1000,
							  (size_t) kb_endpoint_max_body * 1024, &status, &message);
		if (r == HTTP_READ_CLOSED)
			break;
		if (r == HTTP_READ_BAD)
		{
			(void) http_send_simple(s->conn, req->method ? req : NULL, status, cors_headers(),
									message, false);
			break;
		}
		/* a request read after the endpoint was switched off: answered so */
		if (!kb_endpoint_on())
		{
			(void) http_send_simple(s->conn, req, 503, cors_headers(),
									"the SPARQL endpoint is off", false);
			break;
		}
		s->req = req;
		served++;
		serve(req);
		keep_alive = req->keep_alive;
		s->req = NULL;
		s->resp = NULL;
		MemoryContextSwitchTo(TopMemoryContext);
		MemoryContextReset(s->reqcxt);
		if (!keep_alive)
			break;
		if (s->logged_in)
			pgstat_report_activity(STATE_IDLE, NULL);
	}

	if (s->conn)
	{
		http_conn_close(s->conn);
		s->conn = NULL;
	}
	proc_exit(0);
}
