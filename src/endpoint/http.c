/* SPDX-License-Identifier: Apache-2.0 */
/*-------------------------------------------------------------------------
 *
 * http.c
 *	  HTTP/1.1 for one client connection that one single-threaded process
 *	  owns (docs/design.md §5.3): requests read, with their bodies, whole
 *	  or chunked; responses written whole, or chunked once their body
 *	  passes 64 kB.
 *
 * The I/O is the session's own, on its socket or its TLS, and waits in a
 * WaitEventSet on the socket and the process latch, so that signals,
 * cancels and the postmaster's death are seen as a backend sees them, as
 * vexec_flight's h2.c waits (pg_accel/modules/vexec_flight/src/h2.c).
 * Between requests a cancel means nothing (HTTP_WAIT_IDLE), as between a
 * backend's statements, a reload is read, and an endpoint a reload has
 * switched off ends the wait as the idle timeout does; while a request is
 * read or a response written, every interrupt is served but the reload,
 * which waits for the request's end (session.c).
 *
 * A response's head waits for the first 64 kB of its body: a result that
 * fits goes out with its Content-Length, and an error found before then --
 * most are found at once -- is answered with its own status instead.  Past
 * them the body goes out chunked; an error after that can only cut the
 * connection, before the last chunk, which a client sees as a response
 * that did not end.
 *
 *-------------------------------------------------------------------------
 */
#include "postgres.h"

#include <sys/socket.h>

#include "miscadmin.h"
#include "postmaster/interrupt.h"
#include "storage/latch.h"
#include "storage/sinval.h"
#include "storage/waiteventset.h"
#include "tcop/tcopprot.h"
#include "utils/guc.h"
#include "utils/memutils.h"
#include "utils/timestamp.h"
#include "utils/wait_event.h"

#include "endpoint/endpoint.h"

#define HTTP_READ_CHUNK		(64 * 1024)
#define HTTP_MAX_HEAD		(64 * 1024)
#define HTTP_MAX_HEADERS	100
#define HTTP_HOLD			(64 * 1024)

struct HttpConn
{
	int			sock;
	KbTls	   *tls;
	WaitEventSet *wes;
	int			sock_pos;
	MemoryContext mcxt;
	char	   *inbuf;
	size_t		inlen;			/* bytes in inbuf */
	size_t		inpos;			/* the first not consumed */
	size_t		incap;
	bool		dead;
};

struct HttpResponse
{
	HttpConn   *conn;
	bool		head_only;		/* HEAD: no body */
	bool		keep_alive;
	int			status;
	char	   *content_type;
	char	   *headers;		/* "Name: value\r\n"... */
	bool		headed;			/* head_() called */
	bool		started;		/* the head is sent: chunked */
	StringInfoData held;
};

/* ---------------------------------------------------------------------
 * Waiting
 * ---------------------------------------------------------------------
 */

static void
handle_interrupts(HttpWait mode)
{
	if (mode == HTTP_WAIT_IDLE)
	{
		if (ConfigReloadPending)
		{
			ConfigReloadPending = false;
			ProcessConfigFile(PGC_SIGHUP);
		}
		/* no statement runs: a cancel means nothing, as for a backend */
		QueryCancelPending = false;
		if (catchupInterruptPending)
			ProcessCatchupInterrupt();
	}
	CHECK_FOR_INTERRUPTS();
}

/*
 * Wait up to timeout_ms (-1: for ever) for the socket's events, or the
 * latch; the socket's events that came, 0 on a time out or the latch.
 */
static int
conn_wait(HttpConn *conn, int events, long timeout_ms, HttpWait mode)
{
	WaitEvent	ev[3];
	int			n;
	int			got = 0;
	int			i;

	ModifyWaitEvent(conn->wes, conn->sock_pos, events, NULL);
	n = WaitEventSetWait(conn->wes, timeout_ms, ev, lengthof(ev),
						 (events & WL_SOCKET_WRITEABLE) ? WAIT_EVENT_CLIENT_WRITE :
						 WAIT_EVENT_CLIENT_READ);
	for (i = 0; i < n; i++)
	{
		if (ev[i].events & WL_LATCH_SET)
		{
			ResetLatch(MyLatch);
			handle_interrupts(mode);
		}
		if (ev[i].events & (WL_SOCKET_READABLE | WL_SOCKET_WRITEABLE))
			got |= ev[i].events & (WL_SOCKET_READABLE | WL_SOCKET_WRITEABLE);
	}
	return got;
}

/* Bytes read: > 0; 0 at the end; -1 none yet, *wait what to wait for; -2 failed. */
static ssize_t
conn_recv(HttpConn *conn, char *buf, size_t len, int *wait)
{
	ssize_t		n;

	if (conn->tls)
		return kb_tls_read(conn->tls, buf, len, wait);
	n = recv(conn->sock, buf, len, 0);
	if (n >= 0)
		return n;
	if (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR)
	{
		*wait = WL_SOCKET_READABLE;
		return -1;
	}
	return -2;
}

static ssize_t
conn_send(HttpConn *conn, const char *buf, size_t len, int *wait)
{
	ssize_t		n;

	if (conn->tls)
		return kb_tls_write(conn->tls, buf, len, wait);
	n = send(conn->sock, buf, len, MSG_NOSIGNAL);
	if (n >= 0)
		return n;
	if (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR)
	{
		*wait = WL_SOCKET_WRITEABLE;
		return -1;
	}
	return -2;
}

HttpConn *
http_conn_create(int sock, KbTls *tls)
{
	MemoryContext mcxt = AllocSetContextCreate(TopMemoryContext, "pg_knowledge_base connection",
											   ALLOCSET_DEFAULT_SIZES);
	HttpConn   *conn = MemoryContextAllocZero(mcxt, sizeof(HttpConn));

	conn->sock = sock;
	conn->tls = tls;
	conn->mcxt = mcxt;
	if (!pg_set_noblock(sock))
		ereport(FATAL, (errmsg("pg_knowledge_base endpoint could not make its socket non-blocking: %m")));
	conn->wes = CreateWaitEventSet(NULL, 3);
	AddWaitEventToSet(conn->wes, WL_LATCH_SET, PGINVALID_SOCKET, MyLatch, NULL);
	AddWaitEventToSet(conn->wes, WL_EXIT_ON_PM_DEATH, PGINVALID_SOCKET, NULL, NULL);
	conn->sock_pos = AddWaitEventToSet(conn->wes, WL_SOCKET_READABLE, sock, NULL, NULL);
	return conn;
}

bool
http_conn_alive(HttpConn *conn)
{
	return conn != NULL && !conn->dead;
}

void
http_conn_close(HttpConn *conn)
{
	if (conn == NULL)
		return;
	if (conn->tls)
		kb_tls_close(conn->tls);
	if (conn->sock >= 0)
	{
		shutdown(conn->sock, SHUT_WR);
		closesocket(conn->sock);
		conn->sock = -1;
	}
	conn->dead = true;
}

/*
 * More bytes into inbuf, waiting until deadline (0: for ever): > 0 read,
 * 0 the client closed, -1 the time ran out, -2 the connection failed.
 */
static ssize_t
fill(HttpConn *conn, TimestampTz deadline, HttpWait mode)
{
	for (;;)
	{
		int			wait = 0;
		ssize_t		n;
		long		remaining = -1;

		if (conn->inpos > 0 && conn->inpos == conn->inlen)
			conn->inpos = conn->inlen = 0;
		if (conn->incap - conn->inlen < HTTP_READ_CHUNK)
		{
			/* the consumed bytes give their room back first */
			if (conn->inpos > 0)
			{
				memmove(conn->inbuf, conn->inbuf + conn->inpos, conn->inlen - conn->inpos);
				conn->inlen -= conn->inpos;
				conn->inpos = 0;
			}
			if (conn->incap - conn->inlen < HTTP_READ_CHUNK)
			{
				conn->incap = Max(conn->incap * 2, conn->inlen + HTTP_READ_CHUNK);
				conn->inbuf = conn->inbuf ? repalloc_huge(conn->inbuf, conn->incap)
					: MemoryContextAlloc(conn->mcxt, conn->incap);
			}
		}
		n = conn_recv(conn, conn->inbuf + conn->inlen, HTTP_READ_CHUNK, &wait);
		if (n > 0)
		{
			conn->inlen += n;
			return n;
		}
		if (n == 0 || n == -2)
		{
			conn->dead = true;
			return n;
		}
		if (deadline != 0)
		{
			remaining = TimestampDifferenceMilliseconds(GetCurrentTimestamp(), deadline);
			if (remaining <= 0)
				return -1;
		}
		if (conn_wait(conn, wait, remaining, mode) == 0 &&
			mode == HTTP_WAIT_IDLE && !kb_endpoint_on())
			return -1;
	}
}

/* Write len bytes in full: false when the client is gone. */
bool
http_write_raw(HttpConn *conn, const char *data, size_t len, HttpWait mode)
{
	while (len > 0)
	{
		int			wait = 0;
		ssize_t		n;

		if (conn->dead)
			return false;
		n = conn_send(conn, data, len, &wait);
		if (n > 0)
		{
			data += n;
			len -= n;
			continue;
		}
		if (n == -2 || n == 0)
		{
			conn->dead = true;
			return false;
		}
		(void) conn_wait(conn, wait, -1, mode);
	}
	return true;
}

/* ---------------------------------------------------------------------
 * Requests
 * ---------------------------------------------------------------------
 */

static char *
find_head_end(HttpConn *conn)
{
	char	   *p = conn->inbuf + conn->inpos;
	size_t		len = conn->inlen - conn->inpos;
	size_t		i;

	for (i = 0; i + 3 < len; i++)
		if (p[i] == '\r' && p[i + 1] == '\n' && p[i + 2] == '\r' && p[i + 3] == '\n')
			return p + i;
	return NULL;
}

const char *
http_header(const HttpRequest *req, const char *name)
{
	int			i;

	for (i = 0; i < req->nheaders; i++)
		if (strcmp(req->headers[i].name, name) == 0)
			return req->headers[i].value;
	return NULL;
}

static bool
token_in(const char *value, const char *token)
{
	size_t		len = strlen(token);
	const char *p = value;

	while (p && *p)
	{
		while (*p == ' ' || *p == '\t' || *p == ',')
			p++;
		if (pg_strncasecmp(p, token, len) == 0 &&
			(p[len] == '\0' || p[len] == ',' || p[len] == ' ' || p[len] == '\t' || p[len] == ';'))
			return true;
		p = strchr(p, ',');
	}
	return false;
}

static bool
valid_token(const char *s)
{
	if (*s == '\0')
		return false;
	for (; *s; s++)
		if (!(isalnum((unsigned char) *s) || strchr("!#$%&'*+-.^_`|~", *s)))
			return false;
	return true;
}

/* The request's head, parsed in place: false and *message on a bad one. */
static bool
parse_head(char *head, HttpRequest *req, const char **message)
{
	char	   *line = head;
	char	   *eol;
	char	   *sp1;
	char	   *sp2;
	char	   *q;

	eol = strstr(line, "\r\n");
	if (eol == NULL)
	{
		*message = "a request line without an end";
		return false;
	}
	*eol = '\0';
	sp1 = strchr(line, ' ');
	sp2 = sp1 ? strchr(sp1 + 1, ' ') : NULL;
	if (sp1 == NULL || sp2 == NULL || strchr(sp2 + 1, ' ') != NULL)
	{
		*message = "a malformed request line";
		return false;
	}
	*sp1 = '\0';
	*sp2 = '\0';
	req->method = line;
	req->target = sp1 + 1;
	if (!valid_token(req->method))
	{
		*message = "a malformed method";
		return false;
	}
	if (strcmp(sp2 + 1, "HTTP/1.1") == 0)
		req->minor = 1;
	else if (strcmp(sp2 + 1, "HTTP/1.0") == 0)
		req->minor = 0;
	else
	{
		*message = "an HTTP version this endpoint does not speak: HTTP/1.1 or HTTP/1.0";
		return false;
	}
	if (req->target[0] != '/')
	{
		/* an absolute form: its path */
		char	   *scheme_end = strstr(req->target, "://");
		char	   *path = scheme_end ? strchr(scheme_end + 3, '/') : NULL;

		if (path == NULL)
		{
			*message = "a request target that is not a path";
			return false;
		}
		req->target = path;
	}
	req->path = pstrdup(req->target);
	q = strchr(req->path, '?');
	if (q)
	{
		*q = '\0';
		req->query = q + 1;
	}
	else
		req->query = "";
	/* a fragment is never sent; drop one that was */
	q = strchr(req->query, '#');
	if (q)
		*q = '\0';

	req->headers = palloc(sizeof(HttpHeader) * HTTP_MAX_HEADERS);
	req->nheaders = 0;
	line = eol + 2;
	while (*line)
	{
		char	   *colon;
		char	   *value;
		char	   *end;
		char	   *c;
		char	   *next;

		eol = strstr(line, "\r\n");
		if (eol == NULL)
			next = line + strlen(line);
		else
		{
			*eol = '\0';
			next = eol + 2;
		}
		if (*line == ' ' || *line == '\t')
		{
			*message = "a folded header";
			return false;
		}
		colon = strchr(line, ':');
		if (colon == NULL || colon == line)
		{
			*message = "a malformed header";
			return false;
		}
		*colon = '\0';
		for (c = line; *c; c++)
			*c = pg_ascii_tolower((unsigned char) *c);
		if (!valid_token(line))
		{
			*message = "a malformed header name";
			return false;
		}
		value = colon + 1;
		while (*value == ' ' || *value == '\t')
			value++;
		end = value + strlen(value);
		while (end > value && (end[-1] == ' ' || end[-1] == '\t'))
			*--end = '\0';
		if (req->nheaders >= HTTP_MAX_HEADERS)
		{
			*message = "too many headers";
			return false;
		}
		req->headers[req->nheaders].name = line;
		req->headers[req->nheaders].value = value;
		req->nheaders++;
		line = next;
	}
	return true;
}

/* bytes to the body: n of them, from the buffer and the socket */
static bool
read_bytes(HttpConn *conn, StringInfo body, size_t n, TimestampTz deadline)
{
	while (n > 0)
	{
		size_t		have = conn->inlen - conn->inpos;

		if (have == 0)
		{
			ssize_t		got = fill(conn, deadline, HTTP_WAIT_REQUEST);

			if (got <= 0)
				return false;
			continue;
		}
		have = Min(have, n);
		appendBinaryStringInfo(body, conn->inbuf + conn->inpos, (int) have);
		conn->inpos += have;
		n -= have;
	}
	return true;
}

/* a line of a chunked body, into line */
static bool
read_line(HttpConn *conn, StringInfo line, TimestampTz deadline)
{
	resetStringInfo(line);
	for (;;)
	{
		while (conn->inpos < conn->inlen)
		{
			char		c = conn->inbuf[conn->inpos++];

			if (c == '\n')
			{
				if (line->len > 0 && line->data[line->len - 1] == '\r')
					line->data[--line->len] = '\0';
				return true;
			}
			if (line->len > 4096)
				return false;
			appendStringInfoChar(line, c);
		}
		if (fill(conn, deadline, HTTP_WAIT_REQUEST) <= 0)
			return false;
	}
}

HttpRead
http_read_request(HttpConn *conn, HttpRequest *req, int idle_timeout_ms, int request_timeout_ms,
				  size_t max_body, int *status, const char **message)
{
	char	   *end;
	char	   *head;
	size_t		head_len;
	TimestampTz deadline;
	const char *te;
	const char *cl;
	const char *connection;
	const char *expect;
	StringInfoData body;

	memset(req, 0, sizeof(*req));
	*status = 0;
	*message = NULL;

	/* the head: idle until its first byte, then within the request's time */
	deadline = idle_timeout_ms > 0 ?
		TimestampTzPlusMilliseconds(GetCurrentTimestamp(), idle_timeout_ms) : 0;
	while ((end = find_head_end(conn)) == NULL)
	{
		bool		idle = conn->inlen == conn->inpos;
		ssize_t		got;

		if (conn->inlen - conn->inpos > HTTP_MAX_HEAD)
		{
			*status = 431;
			*message = "the request's head is too large";
			return HTTP_READ_BAD;
		}
		got = fill(conn, deadline, idle ? HTTP_WAIT_IDLE : HTTP_WAIT_REQUEST);
		if (got <= 0)
			return HTTP_READ_CLOSED;
		if (idle)
			deadline = TimestampTzPlusMilliseconds(GetCurrentTimestamp(), request_timeout_ms);
	}
	if (deadline == 0 || idle_timeout_ms <= 0)
		deadline = TimestampTzPlusMilliseconds(GetCurrentTimestamp(), request_timeout_ms);

	/* blank lines before a request are allowed (RFC 9112 §2.2) */
	while (conn->inpos + 1 < conn->inlen && conn->inbuf[conn->inpos] == '\r' &&
		   conn->inbuf[conn->inpos + 1] == '\n')
		conn->inpos += 2;
	if ((end = find_head_end(conn)) == NULL)
		return HTTP_READ_CLOSED;
	head_len = end - (conn->inbuf + conn->inpos) + 2;
	head = palloc(head_len + 1);
	memcpy(head, conn->inbuf + conn->inpos, head_len);
	head[head_len] = '\0';
	conn->inpos += head_len + 2;
	if (!parse_head(head, req, message))
	{
		*status = 400;
		return HTTP_READ_BAD;
	}

	connection = http_header(req, "connection");
	if (req->minor == 0)
		req->keep_alive = connection != NULL && token_in(connection, "keep-alive");
	else
		req->keep_alive = connection == NULL || !token_in(connection, "close");

	/* the body: chunked, or of a Content-Length, or none */
	initStringInfo(&body);
	te = http_header(req, "transfer-encoding");
	cl = http_header(req, "content-length");
	expect = http_header(req, "expect");
	if (te != NULL && cl != NULL)
	{
		*status = 400;
		*message = "both Transfer-Encoding and Content-Length";
		req->keep_alive = false;
		return HTTP_READ_BAD;
	}
	if (te != NULL)
	{
		StringInfoData line;

		if (pg_strcasecmp(te, "chunked") != 0)
		{
			*status = 501;
			*message = "a transfer coding other than chunked";
			req->keep_alive = false;
			return HTTP_READ_BAD;
		}
		if (expect != NULL && pg_strcasecmp(expect, "100-continue") == 0 && req->minor == 1)
			(void) http_write_raw(conn, "HTTP/1.1 100 Continue\r\n\r\n", 25, HTTP_WAIT_REQUEST);
		initStringInfo(&line);
		for (;;)
		{
			char	   *endp;
			unsigned long size;

			if (!read_line(conn, &line, deadline))
				return HTTP_READ_CLOSED;
			errno = 0;
			size = strtoul(line.data, &endp, 16);
			if (endp == line.data || errno != 0 || (*endp != '\0' && *endp != ';'))
			{
				*status = 400;
				*message = "a malformed chunk";
				req->keep_alive = false;
				return HTTP_READ_BAD;
			}
			if (size == 0)
				break;
			if ((size_t) body.len + size > max_body)
			{
				*status = 413;
				*message = "the request's body is larger than kb.endpoint_max_body";
				req->keep_alive = false;
				return HTTP_READ_BAD;
			}
			if (!read_bytes(conn, &body, size, deadline) || !read_line(conn, &line, deadline))
				return HTTP_READ_CLOSED;
		}
		/* trailers, to the empty line */
		do
		{
			if (!read_line(conn, &line, deadline))
				return HTTP_READ_CLOSED;
		} while (line.len > 0);
	}
	else if (cl != NULL)
	{
		char	   *endp;
		unsigned long long len;

		errno = 0;
		len = strtoull(cl, &endp, 10);
		if (endp == cl || *endp != '\0' || errno != 0 || !isdigit((unsigned char) cl[0]))
		{
			*status = 400;
			*message = "a malformed Content-Length";
			req->keep_alive = false;
			return HTTP_READ_BAD;
		}
		if (len > max_body)
		{
			*status = 413;
			*message = "the request's body is larger than kb.endpoint_max_body";
			req->keep_alive = false;
			return HTTP_READ_BAD;
		}
		if (len > 0 && conn->inlen - conn->inpos < len && expect != NULL &&
			pg_strcasecmp(expect, "100-continue") == 0 && req->minor == 1)
			(void) http_write_raw(conn, "HTTP/1.1 100 Continue\r\n\r\n", 25, HTTP_WAIT_REQUEST);
		if (len > 0)
		{
			enlargeStringInfo(&body, (int) len);
			if (!read_bytes(conn, &body, len, deadline))
				return HTTP_READ_CLOSED;
		}
	}
	req->body = body.data;
	req->body_len = body.len;
	return HTTP_READ_OK;
}

/* ---------------------------------------------------------------------
 * Responses
 * ---------------------------------------------------------------------
 */

const char *
http_reason(int status)
{
	switch (status)
	{
		case 100:
			return "Continue";
		case 200:
			return "OK";
		case 201:
			return "Created";
		case 204:
			return "No Content";
		case 308:
			return "Permanent Redirect";
		case 400:
			return "Bad Request";
		case 401:
			return "Unauthorized";
		case 403:
			return "Forbidden";
		case 404:
			return "Not Found";
		case 405:
			return "Method Not Allowed";
		case 406:
			return "Not Acceptable";
		case 408:
			return "Request Timeout";
		case 409:
			return "Conflict";
		case 413:
			return "Content Too Large";
		case 415:
			return "Unsupported Media Type";
		case 421:
			return "Misdirected Request";
		case 431:
			return "Request Header Fields Too Large";
		case 500:
			return "Internal Server Error";
		case 501:
			return "Not Implemented";
		case 503:
			return "Service Unavailable";
		default:
			return "Status";
	}
}

static void
append_head(StringInfo s, HttpResponse *resp, bool chunked, size_t length)
{
	appendStringInfo(s, "HTTP/1.1 %d %s\r\n", resp->status, http_reason(resp->status));
	appendStringInfoString(s, "Server: pg_knowledge_base/" "0.1.0" "\r\n");
	if (resp->content_type && resp->content_type[0] && resp->status != 204)
		appendStringInfo(s, "Content-Type: %s\r\n", resp->content_type);
	if (resp->headers)
		appendStringInfoString(s, resp->headers);
	if (resp->status == 204 || resp->status == 304)
		;						/* no body, no length */
	else if (chunked)
		appendStringInfoString(s, "Transfer-Encoding: chunked\r\n");
	else
		appendStringInfo(s, "Content-Length: %zu\r\n", length);
	if (!resp->keep_alive)
		appendStringInfoString(s, "Connection: close\r\n");
	appendStringInfoString(s, "\r\n");
}

HttpResponse *
http_response_create(HttpConn *conn, const HttpRequest *req)
{
	HttpResponse *resp = palloc0(sizeof(HttpResponse));

	resp->conn = conn;
	resp->head_only = req->method && strcmp(req->method, "HEAD") == 0;
	resp->keep_alive = req->keep_alive;
	resp->status = 200;
	initStringInfo(&resp->held);
	return resp;
}

void
http_response_head(HttpResponse *resp, int status, const char *content_type, const char *headers)
{
	resp->status = status;
	resp->content_type = content_type ? pstrdup(content_type) : NULL;
	if (headers && headers[0])
		resp->headers = resp->headers ? psprintf("%s%s", resp->headers, headers) : pstrdup(headers);
	resp->headed = true;
}

/* The status the response's head gives, 0 before it has one */
int
http_response_status(HttpResponse *resp)
{
	return resp != NULL && resp->headed ? resp->status : 0;
}

bool
http_response_started(HttpResponse *resp)
{
	return resp->started;
}

/* The held bytes as one chunk */
static bool
send_chunk(HttpResponse *resp, const char *data, size_t len)
{
	char		size[32];
	int			n;

	if (len == 0)
		return true;
	n = snprintf(size, sizeof(size), "%zx\r\n", len);
	return http_write_raw(resp->conn, size, n, HTTP_WAIT_RESPONSE) &&
		http_write_raw(resp->conn, data, len, HTTP_WAIT_RESPONSE) &&
		http_write_raw(resp->conn, "\r\n", 2, HTTP_WAIT_RESPONSE);
}

bool
http_response_write(HttpResponse *resp, const char *data, size_t len)
{
	if (resp->head_only || resp->status == 204)
		return true;
	if (!resp->started)
	{
		appendBinaryStringInfo(&resp->held, data, (int) len);
		if (resp->held.len < HTTP_HOLD)
			return true;
		/* past what is held: the head, chunked, then the held bytes */
		{
			StringInfoData head;
			bool		ok;

			initStringInfo(&head);
			append_head(&head, resp, true, 0);
			resp->started = true;
			ok = http_write_raw(resp->conn, head.data, head.len, HTTP_WAIT_RESPONSE) &&
				send_chunk(resp, resp->held.data, resp->held.len);
			pfree(head.data);
			resetStringInfo(&resp->held);
			return ok;
		}
	}
	return send_chunk(resp, data, len);
}

/* The response's end: the whole of it, or its last chunk. */
bool
http_response_end(HttpResponse *resp)
{
	if (!resp->started)
	{
		StringInfoData out;
		bool		ok;

		initStringInfo(&out);
		append_head(&out, resp, false, resp->head_only ? 0 : resp->held.len);
		if (!resp->head_only && resp->status != 204)
			appendBinaryStringInfo(&out, resp->held.data, resp->held.len);
		ok = http_write_raw(resp->conn, out.data, out.len, HTTP_WAIT_RESPONSE);
		pfree(out.data);
		resp->started = true;
		return ok;
	}
	return http_write_raw(resp->conn, "0\r\n\r\n", 5, HTTP_WAIT_RESPONSE);
}

/* The held body dropped, as for an error found before the head went out. */
void
http_response_discard(HttpResponse *resp)
{
	resetStringInfo(&resp->held);
	resp->headers = NULL;
	resp->headed = false;
}

/* A whole response of a short text. */
bool
http_send_simple(HttpConn *conn, const HttpRequest *req, int status, const char *headers,
				 const char *message, bool keep_alive)
{
	StringInfoData out;
	size_t		len = message ? strlen(message) + 1 : 0;
	bool		head_only = req && req->method && strcmp(req->method, "HEAD") == 0;
	bool		ok;

	initStringInfo(&out);
	appendStringInfo(&out, "HTTP/1.1 %d %s\r\n", status, http_reason(status));
	appendStringInfoString(&out, "Server: pg_knowledge_base/0.1.0\r\n");
	if (message)
		appendStringInfoString(&out, "Content-Type: text/plain; charset=utf-8\r\n");
	if (headers)
		appendStringInfoString(&out, headers);
	if (status != 204)
		appendStringInfo(&out, "Content-Length: %zu\r\n", len);
	if (!keep_alive)
		appendStringInfoString(&out, "Connection: close\r\n");
	appendStringInfoString(&out, "\r\n");
	if (message && !head_only && status != 204)
	{
		appendStringInfoString(&out, message);
		appendStringInfoChar(&out, '\n');
	}
	ok = http_write_raw(conn, out.data, out.len, HTTP_WAIT_RESPONSE);
	pfree(out.data);
	return ok;
}
