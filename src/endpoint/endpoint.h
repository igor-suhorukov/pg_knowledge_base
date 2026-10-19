/* SPDX-License-Identifier: Apache-2.0 */
/*-------------------------------------------------------------------------
 *
 * endpoint.h
 *	  The SPARQL endpoint (docs/design.md §5): SPARQL 1.1's protocol and
 *	  graph store protocol over HTTP/1.1, served by PostgreSQL's processes
 *	  as pg_accel's Flight SQL endpoint (vexec_flight) serves Flight: an
 *	  acceptor without threads, a session -- a dynamic background worker --
 *	  for each connection, owning its socket as a backend does, logins
 *	  checked as PostgreSQL checks a backend's.
 *
 * The parts:
 *
 *	settings.c	the settings and the shared table of sessions, the
 *				acceptor's registration
 *	acceptor.c	the acceptor: listens on the endpoint's port, asks the
 *				postmaster for a session worker for each connection and
 *				hands it the socket, and checks the sessions' logins
 *	login.c		pg_hba.conf, passwords and connection limits, for a login,
 *				with the client's real address
 *	hba.c		the acceptor's own copy of pg_hba.conf's lines, and of
 *				PostgreSQL's match against them
 *	session.c	a session: one dynamic background worker, one thread, one
 *				connection; its requests, each a transaction
 *	http.c		HTTP/1.1 on the session's socket or its TLS: requests read,
 *				responses written whole or chunked
 *	tls.c		the session's OpenSSL context, ALPN "http/1.1"
 *
 * What a request asks, and its answer, are the engine's
 * (engine/src/http.rs), as oxigraph's server's are.
 *
 *-------------------------------------------------------------------------
 */
#ifndef KB_ENDPOINT_H
#define KB_ENDPOINT_H

#include <sys/socket.h>

#include "datatype/timestamp.h"
#include "lib/stringinfo.h"
#include "storage/spin.h"

/* settings.c: the settings */
extern char *kb_endpoint_listen_addresses;
extern int	kb_endpoint_port;
extern char *kb_endpoint_ssl_cert_file;
extern char *kb_endpoint_ssl_key_file;
extern int	kb_endpoint_max_sessions;
extern int	kb_endpoint_idle_session_timeout;
extern char *kb_endpoint_anonymous_role;
extern bool kb_endpoint_read_only;
extern int	kb_endpoint_max_body;
extern bool kb_endpoint_cors;

extern void kb_endpoint_define_settings(void);
extern void kb_endpoint_register(void);
extern bool kb_endpoint_on(void);

/*
 * The sessions, in shared memory: a slot each.  The acceptor reserves one
 * before it asks for a worker, so that kb.endpoint_max_sessions holds; the
 * session fills it as it logs in, and frees it when it exits.  Logins count
 * a role's and a database's sessions here, since PostgreSQL counts
 * backends only.
 */
typedef enum EndpointSlotState
{
	ENDPOINT_SLOT_FREE,
	ENDPOINT_SLOT_RESERVED,		/* the acceptor asked for a worker */
	ENDPOINT_SLOT_ATTACHED,		/* the worker has the socket */
	ENDPOINT_SLOT_ACTIVE		/* logged in and connected */
} EndpointSlotState;

typedef struct EndpointSlot
{
	int			state;			/* EndpointSlotState */
	pid_t		pid;
	Oid			roleid;
	Oid			dboid;
	uint64		token;			/* the hand-over's */
	TimestampTz since;
} EndpointSlot;

typedef struct EndpointShared
{
	slock_t		mutex;
	int			nslots;
	pid_t		acceptor_pid;
	EndpointSlot slots[FLEXIBLE_ARRAY_MEMBER];
} EndpointShared;

extern EndpointShared *kb_endpoint_shared;

extern int	kb_endpoint_count_sessions(Oid roleid, Oid dboid);

/*
 * The acceptor's UNIX-domain socket, by which a session takes its client's
 * socket and asks for its login to be checked: in the data directory, which
 * only the server's user may enter, named by the port.
 */
#define ENDPOINT_SOCKET_NAME	"kb_endpoint.%d.sock"
#define ENDPOINT_MAGIC			0x4B425331	/* "KBS1" */
#define ENDPOINT_MAX_PASSWORD	1024

typedef struct EndpointHello	/* session -> acceptor, first */
{
	uint32		magic;
	int			slot;
	uint64		token;
} EndpointHello;

typedef struct EndpointLoginRequest /* session -> acceptor */
{
	uint32		magic;
	int			slot;
	char		user[NAMEDATALEN];
	char		database[NAMEDATALEN];
	char		password[ENDPOINT_MAX_PASSWORD];
	bool		has_password;	/* false: no credentials, the anonymous role */
	bool		ssl;
	struct sockaddr_storage addr;
	socklen_t	addrlen;
} EndpointLoginRequest;

typedef struct EndpointLoginReply	/* acceptor -> session */
{
	uint32		magic;
	bool		ok;
	Oid			roleid;
	Oid			dboid;
	int			http_status;
	bool		challenge;		/* answer with WWW-Authenticate: credentials may help */
	char		sqlstate[6];
	char		message[512];
} EndpointLoginReply;

/* acceptor.c */
extern PGDLLEXPORT void kb_endpoint_acceptor_main(Datum arg);

/* login.c */
extern void kb_endpoint_login_check(const EndpointLoginRequest *req, EndpointLoginReply *reply);

/* hba.c */
struct Port;
extern bool kb_endpoint_load_hba(void);
extern void kb_endpoint_check_hba(struct Port *port);

/* session.c */
extern PGDLLEXPORT void kb_endpoint_session_main(Datum arg);

/* tls.c */
typedef struct KbTls KbTls;

extern KbTls *kb_tls_create(void);
extern bool kb_tls_accept(KbTls *tls, int sock, int timeout_ms);
extern void *kb_tls_ssl(KbTls *tls);
extern ssize_t kb_tls_read(KbTls *tls, void *buf, size_t len, int *wait_events);
extern ssize_t kb_tls_write(KbTls *tls, const void *buf, size_t len, int *wait_events);
extern void kb_tls_close(KbTls *tls);

/* http.c */
typedef struct HttpConn HttpConn;

typedef struct HttpHeader
{
	char	   *name;			/* lower case */
	char	   *value;
} HttpHeader;

typedef struct HttpRequest
{
	char	   *method;
	char	   *target;
	char	   *path;			/* percent-decoded? no: as sent */
	char	   *query;			/* after "?", or "" */
	int			minor;			/* HTTP/1.minor */
	HttpHeader *headers;
	int			nheaders;
	char	   *body;
	size_t		body_len;
	bool		keep_alive;
} HttpRequest;

/* What reading a request gave */
typedef enum HttpRead
{
	HTTP_READ_OK,
	HTTP_READ_CLOSED,			/* the client closed, or went quiet */
	HTTP_READ_BAD				/* answer *status, then close */
} HttpRead;

typedef enum HttpWait
{
	HTTP_WAIT_IDLE,				/* between requests: a cancel means nothing */
	HTTP_WAIT_REQUEST,			/* a request's bytes */
	HTTP_WAIT_RESPONSE			/* a response's: every interrupt */
} HttpWait;

extern HttpConn *http_conn_create(int sock, KbTls *tls);
extern bool http_conn_alive(HttpConn *conn);
extern void http_conn_close(HttpConn *conn);
extern HttpRead http_read_request(HttpConn *conn, HttpRequest *req, int idle_timeout_ms,
								  int request_timeout_ms, size_t max_body, int *status,
								  const char **message);
extern const char *http_header(const HttpRequest *req, const char *name);

/* A response: its head held until the body's first 64 kB are known */
typedef struct HttpResponse HttpResponse;

extern HttpResponse *http_response_create(HttpConn *conn, const HttpRequest *req);
extern void http_response_head(HttpResponse *resp, int status, const char *content_type,
							   const char *headers);
extern bool http_response_write(HttpResponse *resp, const char *data, size_t len);
extern bool http_response_end(HttpResponse *resp);
extern bool http_response_started(HttpResponse *resp);
extern int	http_response_status(HttpResponse *resp);
extern void http_response_discard(HttpResponse *resp);
extern bool http_send_simple(HttpConn *conn, const HttpRequest *req, int status,
							 const char *headers, const char *message, bool keep_alive);
extern bool http_write_raw(HttpConn *conn, const char *data, size_t len, HttpWait mode);
extern const char *http_reason(int status);

#endif							/* KB_ENDPOINT_H */
