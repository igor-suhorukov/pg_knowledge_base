/* SPDX-License-Identifier: Apache-2.0 */
/*-------------------------------------------------------------------------
 *
 * acceptor.c
 *	  The acceptor: a background worker without threads that listens on the
 *	  SPARQL endpoint's port, and starts a session for each connection
 *	  (docs/design.md §5.1) -- vexec_flight's acceptor
 *	  (pg_accel/modules/vexec_flight/src/acceptor.c), the names changed.
 *
 * The postmaster accepts a connection and forks a backend that owns its
 * socket (PG19:src/backend/postmaster/postmaster.c:3584).  An extension
 * cannot fork from the postmaster, so the acceptor does what it can: for
 * each connection it reserves a slot of the session table, asks the
 * postmaster for a session worker (RegisterDynamicBackgroundWorker,
 * PG19:src/include/postmaster/bgworker.h:125) with the slot and a token in
 * bgw_extra, and hands the worker the socket when it connects to the
 * acceptor's UNIX-domain socket and shows the token (SCM_RIGHTS).  The
 * session then owns the socket, as a backend does.  A connection the
 * acceptor cannot give a session -- kb.endpoint_max_sessions are open, or
 * no background worker slot is free -- is answered 503 and closed.
 *
 * The acceptor also checks the sessions' logins (login.c): it is connected
 * to no database, and reads the shared catalogs a login needs, and its own
 * copy of pg_hba.conf, which a background worker does not inherit -- the
 * postmaster's parsed lines are freed when a worker starts
 * (PG19:src/backend/postmaster/bgworker.c:748-751), so the acceptor reads
 * them again with PostgreSQL's parser into a context of its own (hba.c).  A
 * session asks once it has read its client's credentials, and connects only
 * to the role and the database the acceptor names.
 *
 * Switched on and off.  The acceptor runs whenever the module is preloaded,
 * and listens on kb.endpoint_listen_addresses, which a reload sets again:
 * between two turns of its loop it closes the sockets of the list before
 * and listens on the new one -- none, when it is empty, and the endpoint is
 * off.  It goes on checking logins and handing sockets over, for the
 * connections it accepted before; the sessions close theirs (session.c).
 * The acceptor never exits for it: a worker that exits with 0 is
 * unregistered, and could not switch the endpoint on again
 * (PG19:doc/src/sgml/bgworker.sgml:253-261).
 *
 *-------------------------------------------------------------------------
 */
#include "postgres.h"

#include <netinet/in.h>
#include <netinet/tcp.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

#include "common/ip.h"
#include "libpq/hba.h"
#include "libpq/libpq.h"
#include "miscadmin.h"
#include "postmaster/bgworker.h"
#include "postmaster/interrupt.h"
#include "postmaster/postmaster.h"
#include "storage/ipc.h"
#include "storage/latch.h"
#include "storage/waiteventset.h"
#include "tcop/tcopprot.h"
#include "utils/guc.h"
#include "utils/memutils.h"
#include "utils/timestamp.h"
#include "utils/varlena.h"
#include "utils/wait_event.h"

#include "endpoint/endpoint.h"

#define MAX_LISTEN			16
#define HANDOVER_TIMEOUT_MS	60000
#define RELISTEN_MS			5000

/* A connection waiting for its session worker. */
typedef struct Pending
{
	bool		used;
	int			fd;
	int			slot;
	uint64		token;
	TimestampTz since;
	BackgroundWorkerHandle *handle;
} Pending;

/* A session worker's connection to the acceptor. */
typedef struct SessionLink
{
	bool		used;
	int			fd;
	bool		helloed;		/* it showed its token, and has its socket */
	int			slot;
	char		buf[sizeof(EndpointLoginRequest)];
	size_t		len;
	TimestampTz since;
} SessionLink;

/*
 * A refused connection, answered and half closed: its request read and
 * dropped until the client closes, or LINGER_MS pass, so that closing with
 * its bytes unread does not reset the connection before the client reads
 * the answer.
 */
typedef struct Lingering
{
	bool		used;
	int			fd;
	TimestampTz since;
} Lingering;

#define MAX_LINGERING		64
#define LINGER_MS			2000

static Lingering lingering[MAX_LINGERING];

static pgsocket listen_fds[MAX_LISTEN];
static int	nlisten = 0;
static char *listening_for = NULL;	/* the list listen_fds are for */
static TimestampTz listen_tried = 0;
static int	unix_fd = -1;
static char unix_path[MAXPGPATH];
static Pending *pending;
static SessionLink *links;
static int	nlinks;
static MemoryContext loopcxt;

static void
remove_unix_socket(int code, Datum arg)
{
	(void) code;
	(void) arg;
	if (unix_path[0])
		unlink(unix_path);
}

/* pg_hba.conf, read again (hba.c): at the start, and on SIGHUP. */
static void
reload_hba(void)
{
	if (!kb_endpoint_load_hba())
		ereport(LOG,
				(errmsg("pg_knowledge_base endpoint: pg_hba.conf was not reloaded"),
				 errdetail("Endpoint logins keep the lines read before.")));
}

/*
 * The endpoint's port on each of kb.endpoint_listen_addresses, listened on
 * again when the list has changed: at the start, and between two turns of
 * the loop, outside any wait set, so that no event of a socket closed here
 * is taken for one of a socket that has its number since.  The sockets of
 * the list before are closed first, and the port is bound again
 * (ListenServerPort sets SO_REUSEADDR).  An empty list listens on none: the
 * endpoint is off.  A list none of whose addresses can be listened on
 * leaves it off too, logged, and is tried again every RELISTEN_MS, as the
 * worker's restarts did when that was fatal.
 */
static void
listen_endpoint(void)
{
	bool		changed = listening_for == NULL ||
		strcmp(listening_for, kb_endpoint_listen_addresses) != 0;
	char	   *raw;
	List	   *elems;
	ListCell   *lc;
	int			i;

	if (!changed &&
		(nlisten > 0 || listening_for[0] == '\0' ||
		 !TimestampDifferenceExceeds(listen_tried, GetCurrentTimestamp(), RELISTEN_MS)))
		return;
	for (i = 0; i < nlisten; i++)
		closesocket(listen_fds[i]);
	nlisten = 0;
	if (changed)
	{
		if (listening_for != NULL && kb_endpoint_listen_addresses[0] == '\0')
			ereport(LOG,
					(errmsg("pg_knowledge_base endpoint is off: \"kb.endpoint_listen_addresses\" is empty")));
		if (listening_for != NULL)
			pfree(listening_for);
		listening_for = MemoryContextStrdup(TopMemoryContext, kb_endpoint_listen_addresses);
	}
	if (listening_for[0] == '\0')
		return;
	listen_tried = GetCurrentTimestamp();
	raw = pstrdup(listening_for);
	/* the setting's check refuses a list that does not split */
	if (!SplitGUCList(raw, ',', &elems))
		elems = NIL;
	foreach(lc, elems)
	{
		char	   *host = lfirst(lc);

		(void) ListenServerPort(AF_UNSPEC, strcmp(host, "*") == 0 ? NULL : host,
								(unsigned short) kb_endpoint_port, NULL,
								listen_fds, &nlisten, MAX_LISTEN);
	}
	if (nlisten == 0 && changed)
		ereport(LOG,
				(errmsg("pg_knowledge_base endpoint could not listen on any of \"%s\", port %d",
						listening_for, kb_endpoint_port),
				 errdetail("The endpoint is off; it tries again every %d s.", RELISTEN_MS / 1000)));
}

/* The acceptor's UNIX-domain socket, in the data directory. */
static void
listen_sessions(void)
{
	struct sockaddr_un addr;

	snprintf(unix_path, sizeof(unix_path), ENDPOINT_SOCKET_NAME, kb_endpoint_port);
	memset(&addr, 0, sizeof(addr));
	addr.sun_family = AF_UNIX;
	if (strlen(unix_path) >= sizeof(addr.sun_path))
		elog(FATAL, "pg_knowledge_base endpoint: the socket path \"%s\" is too long", unix_path);
	strlcpy(addr.sun_path, unix_path, sizeof(addr.sun_path));
	unlink(unix_path);
	unix_fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
	if (unix_fd < 0 ||
		bind(unix_fd, (struct sockaddr *) &addr, sizeof(addr)) < 0 ||
		chmod(unix_path, S_IRUSR | S_IWUSR) < 0 ||
		listen(unix_fd, 64) < 0)
		ereport(FATAL,
				(errcode_for_socket_access(),
				 errmsg("pg_knowledge_base endpoint could not listen on \"%s\": %m", unix_path)));
	before_shmem_exit(remove_unix_socket, 0);
}

static void
free_slot(int slot)
{
	SpinLockAcquire(&kb_endpoint_shared->mutex);
	if (kb_endpoint_shared->slots[slot].state == ENDPOINT_SLOT_RESERVED)
		kb_endpoint_shared->slots[slot].state = ENDPOINT_SLOT_FREE;
	SpinLockRelease(&kb_endpoint_shared->mutex);
}

static uint64
random_token(void)
{
	uint64		t;

	if (!pg_strong_random(&t, sizeof(t)))
		elog(ERROR, "pg_knowledge_base endpoint could not make a random token");
	return t;
}

/*
 * A connection no session can take: a 503, written if the socket takes it
 * at once -- a plain request reads it, a TLS client sees the connection
 * close -- and closed.
 */
static void
refuse_client(int fd, const char *why)
{
	char		response[512];
	int			len;

	len = snprintf(response, sizeof(response),
				   "HTTP/1.1 503 Service Unavailable\r\n"
				   "Content-Type: text/plain; charset=utf-8\r\n"
				   "Content-Length: %d\r\n"
				   "Retry-After: 1\r\n"
				   "Connection: close\r\n\r\n%s\n",
				   (int) strlen(why) + 1, why);
	(void) send(fd, response, len, MSG_NOSIGNAL | MSG_DONTWAIT);
	(void) shutdown(fd, SHUT_WR);
	for (int i = 0; i < MAX_LINGERING; i++)
		if (!lingering[i].used)
		{
			(void) pg_set_noblock(fd);
			lingering[i].used = true;
			lingering[i].fd = fd;
			lingering[i].since = GetCurrentTimestamp();
			return;
		}
	closesocket(fd);
}

/* A lingering connection's input dropped; closed at its end or its time. */
static void
drain_lingering(Lingering *l)
{
	char		buf[4096];
	ssize_t		n;

	while ((n = recv(l->fd, buf, sizeof(buf), MSG_DONTWAIT)) > 0)
		;
	if (n == 0 || (errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR))
	{
		closesocket(l->fd);
		l->used = false;
	}
}

/* A client's connection: a slot, a session worker, and the socket held for it. */
static void
accept_client(pgsocket lfd)
{
	struct sockaddr_storage addr;
	socklen_t	addrlen = sizeof(addr);
	int			fd;
	int			slot = -1;
	int			p;
	uint64		token;
	BackgroundWorker worker;
	BackgroundWorkerHandle *handle;

	fd = accept4(lfd, (struct sockaddr *) &addr, &addrlen, SOCK_CLOEXEC);
	if (fd < 0)
		return;

	/*
	 * As pq_init() sets a backend's TCP connection
	 * (PG19:src/backend/libpq/pqcomm.c:207-222): without TCP_NODELAY, a
	 * response's last frames wait for the client's delayed ACK, some 40 ms a
	 * call.
	 */
	{
		int			on = 1;

		if (setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &on, sizeof(on)) < 0 ||
			setsockopt(fd, SOL_SOCKET, SO_KEEPALIVE, &on, sizeof(on)) < 0)
		{
			ereport(LOG,
					(errmsg("pg_knowledge_base endpoint: %s failed on a client's connection: %m", "setsockopt")));
			closesocket(fd);
			return;
		}
	}

	for (p = 0; p < kb_endpoint_max_sessions && pending[p].used; p++)
		;
	token = random_token();
	SpinLockAcquire(&kb_endpoint_shared->mutex);
	if (p < kb_endpoint_max_sessions)
	{
		int			i;

		for (i = 0; i < kb_endpoint_shared->nslots; i++)
			if (kb_endpoint_shared->slots[i].state == ENDPOINT_SLOT_FREE)
			{
				slot = i;
				memset(&kb_endpoint_shared->slots[i], 0, sizeof(EndpointSlot));
				kb_endpoint_shared->slots[i].state = ENDPOINT_SLOT_RESERVED;
				kb_endpoint_shared->slots[i].token = token;
				kb_endpoint_shared->slots[i].since = GetCurrentTimestamp();
				break;
			}
	}
	SpinLockRelease(&kb_endpoint_shared->mutex);
	if (slot < 0)
	{
		ereport(LOG,
				(errmsg("pg_knowledge_base endpoint: a connection was refused: \"kb.endpoint_max_sessions\" (%d) sessions are open",
						kb_endpoint_max_sessions)));
		refuse_client(fd, "kb.endpoint_max_sessions sessions are open");
		return;
	}

	memset(&worker, 0, sizeof(worker));
	worker.bgw_flags = BGWORKER_SHMEM_ACCESS | BGWORKER_BACKEND_DATABASE_CONNECTION;
	worker.bgw_start_time = BgWorkerStart_RecoveryFinished;
	worker.bgw_restart_time = BGW_NEVER_RESTART;
	strlcpy(worker.bgw_library_name, "pg_knowledge_base", BGW_MAXLEN);
	strlcpy(worker.bgw_function_name, "kb_endpoint_session_main", BGW_MAXLEN);
	strlcpy(worker.bgw_name, "pg_knowledge_base endpoint session", BGW_MAXLEN);
	strlcpy(worker.bgw_type, "pg_knowledge_base session", BGW_MAXLEN);
	worker.bgw_main_arg = Int32GetDatum(slot);
	memcpy(worker.bgw_extra, &token, sizeof(token));
	worker.bgw_notify_pid = MyProcPid;
	{
		/* the handle outlives the loop's memory */
		MemoryContext old = MemoryContextSwitchTo(TopMemoryContext);
		bool		registered = RegisterDynamicBackgroundWorker(&worker, &handle);

		MemoryContextSwitchTo(old);
		if (!registered)
		{
			ereport(LOG,
					(errmsg("pg_knowledge_base endpoint: a connection was refused: no background worker slot is free"),
					 errhint("Raise \"max_worker_processes\".")));
			free_slot(slot);
			refuse_client(fd, "no background worker slot is free");
			return;
		}
	}
	pending[p].used = true;
	pending[p].fd = fd;
	pending[p].slot = slot;
	pending[p].token = token;
	pending[p].since = GetCurrentTimestamp();
	pending[p].handle = handle;
}

/* A session worker connects: it will show its token. */
static void
accept_session(void)
{
	int			fd = accept4(unix_fd, NULL, NULL, SOCK_CLOEXEC | SOCK_NONBLOCK);
	int			i;

	if (fd < 0)
		return;
	for (i = 0; i < nlinks && links[i].used; i++)
		;
	if (i == nlinks)
	{
		closesocket(fd);
		return;
	}
	memset(&links[i], 0, sizeof(SessionLink));
	links[i].used = true;
	links[i].fd = fd;
	links[i].since = GetCurrentTimestamp();
}

static void
close_link(SessionLink *link)
{
	closesocket(link->fd);
	link->used = false;
}

/* The client's socket, to the worker that showed its slot's token. */
static void
hand_over(SessionLink *link, const EndpointHello *hello)
{
	int			p;

	for (p = 0; p < kb_endpoint_max_sessions; p++)
		if (pending[p].used && pending[p].slot == hello->slot && pending[p].token == hello->token)
			break;
	if (hello->magic != ENDPOINT_MAGIC || p == kb_endpoint_max_sessions)
	{
		close_link(link);
		return;
	}
	{
		struct msghdr msg;
		struct iovec iov;
		char		ok = 1;
		union
		{
			struct cmsghdr hdr;
			char		buf[CMSG_SPACE(sizeof(int))];
		}			control;
		struct cmsghdr *cmsg;

		memset(&msg, 0, sizeof(msg));
		memset(&control, 0, sizeof(control));
		iov.iov_base = &ok;
		iov.iov_len = 1;
		msg.msg_iov = &iov;
		msg.msg_iovlen = 1;
		msg.msg_control = control.buf;
		msg.msg_controllen = sizeof(control.buf);
		cmsg = CMSG_FIRSTHDR(&msg);
		cmsg->cmsg_level = SOL_SOCKET;
		cmsg->cmsg_type = SCM_RIGHTS;
		cmsg->cmsg_len = CMSG_LEN(sizeof(int));
		memcpy(CMSG_DATA(cmsg), &pending[p].fd, sizeof(int));
		if (sendmsg(link->fd, &msg, MSG_NOSIGNAL) != 1)
		{
			close_link(link);
			return;
		}
	}
	closesocket(pending[p].fd);
	pending[p].used = false;
	pfree(pending[p].handle);
	link->helloed = true;
	link->slot = hello->slot;
	link->len = 0;
}

/* Bytes from a session's link: its hello, then its logins, a whole struct each. */
static void
read_link(SessionLink *link)
{
	size_t		want = link->helloed ? sizeof(EndpointLoginRequest) : sizeof(EndpointHello);
	ssize_t		n = recv(link->fd, link->buf + link->len, want - link->len, 0);

	if (n == 0 || (n < 0 && errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR))
	{
		close_link(link);
		return;
	}
	if (n < 0)
		return;
	link->len += n;
	if (link->len < want)
		return;
	link->len = 0;
	if (!link->helloed)
	{
		EndpointHello hello;

		memcpy(&hello, link->buf, sizeof(hello));
		hand_over(link, &hello);
	}
	else
	{
		EndpointLoginRequest *req = (EndpointLoginRequest *) link->buf;
		EndpointLoginReply reply;

		if (req->magic != ENDPOINT_MAGIC || req->slot != link->slot)
		{
			close_link(link);
			return;
		}
		kb_endpoint_login_check(req, &reply);
		explicit_bzero(link->buf, sizeof(link->buf));
		if (send(link->fd, &reply, sizeof(reply), MSG_NOSIGNAL) != sizeof(reply))
			close_link(link);
	}
}

/* Connections whose worker never came, or died before it took them. */
static void
expire_pending(void)
{
	TimestampTz now = GetCurrentTimestamp();
	int			p;
	int			i;

	for (p = 0; p < kb_endpoint_max_sessions; p++)
	{
		pid_t		pid;

		if (!pending[p].used)
			continue;
		if (GetBackgroundWorkerPid(pending[p].handle, &pid) == BGWH_STOPPED ||
			TimestampDifferenceExceeds(pending[p].since, now, HANDOVER_TIMEOUT_MS))
		{
			closesocket(pending[p].fd);
			free_slot(pending[p].slot);
			pfree(pending[p].handle);
			pending[p].used = false;
		}
	}
	for (i = 0; i < nlinks; i++)
		if (links[i].used && !links[i].helloed &&
			TimestampDifferenceExceeds(links[i].since, now, HANDOVER_TIMEOUT_MS))
			close_link(&links[i]);
	for (i = 0; i < MAX_LINGERING; i++)
		if (lingering[i].used && TimestampDifferenceExceeds(lingering[i].since, now, LINGER_MS))
		{
			closesocket(lingering[i].fd);
			lingering[i].used = false;
		}
}

void
kb_endpoint_acceptor_main(Datum arg)
{
	(void) arg;

	pqsignal(SIGHUP, SignalHandlerForConfigReload);
	BackgroundWorkerUnblockSignals();

	SpinLockAcquire(&kb_endpoint_shared->mutex);
	kb_endpoint_shared->acceptor_pid = MyProcPid;
	SpinLockRelease(&kb_endpoint_shared->mutex);

	/* the shared catalogs, for logins; no database */
	BackgroundWorkerInitializeConnection(NULL, NULL, 0);
	reload_hba();

	pending = MemoryContextAllocZero(TopMemoryContext, sizeof(Pending) * kb_endpoint_max_sessions);
	nlinks = kb_endpoint_max_sessions * 2;
	links = MemoryContextAllocZero(TopMemoryContext, sizeof(SessionLink) * nlinks);
	listen_endpoint();
	listen_sessions();
	loopcxt = AllocSetContextCreate(TopMemoryContext, "pg_knowledge_base acceptor loop",
									ALLOCSET_DEFAULT_SIZES);

	for (;;)
	{
		WaitEventSet *wes;
		WaitEvent	ev[MAX_LISTEN + 64];
		int			n;
		int			i;

		MemoryContextReset(loopcxt);
		MemoryContextSwitchTo(loopcxt);

		/* the set, made again each turn: its sockets come and go */
		wes = CreateWaitEventSet(NULL, 2 + nlisten + 1 + nlinks + MAX_LINGERING);
		AddWaitEventToSet(wes, WL_LATCH_SET, PGINVALID_SOCKET, MyLatch, NULL);
		AddWaitEventToSet(wes, WL_EXIT_ON_PM_DEATH, PGINVALID_SOCKET, NULL, NULL);
		for (i = 0; i < nlisten; i++)
			AddWaitEventToSet(wes, WL_SOCKET_ACCEPT, listen_fds[i], NULL, NULL);
		AddWaitEventToSet(wes, WL_SOCKET_ACCEPT, unix_fd, NULL, NULL);
		for (i = 0; i < nlinks; i++)
			if (links[i].used)
				AddWaitEventToSet(wes, WL_SOCKET_READABLE, links[i].fd, NULL, &links[i]);
		for (i = 0; i < MAX_LINGERING; i++)
			if (lingering[i].used)
				AddWaitEventToSet(wes, WL_SOCKET_READABLE, lingering[i].fd, NULL, NULL);

		n = WaitEventSetWait(wes, 1000, ev, lengthof(ev), PG_WAIT_EXTENSION);
		for (i = 0; i < n; i++)
		{
			if (ev[i].events & WL_LATCH_SET)
			{
				ResetLatch(MyLatch);
				CHECK_FOR_INTERRUPTS();
				if (ConfigReloadPending)
				{
					ConfigReloadPending = false;
					ProcessConfigFile(PGC_SIGHUP);
					reload_hba();
				}
			}
			else if (ev[i].events & (WL_SOCKET_READABLE | WL_SOCKET_ACCEPT))
			{
				/* WL_SOCKET_ACCEPT is WL_SOCKET_READABLE here: told apart by socket */
				int			l;

				if (ev[i].fd == unix_fd)
				{
					accept_session();
					continue;
				}
				for (l = 0; l < nlisten; l++)
					if (ev[i].fd == listen_fds[l])
						break;
				if (l < nlisten)
					accept_client(listen_fds[l]);
				else if (ev[i].user_data != NULL && ((SessionLink *) ev[i].user_data)->used)
					read_link((SessionLink *) ev[i].user_data);
				else
				{
					for (l = 0; l < MAX_LINGERING; l++)
						if (lingering[l].used && lingering[l].fd == ev[i].fd)
							drain_lingering(&lingering[l]);
				}
			}
		}
		FreeWaitEventSet(wes);
		expire_pending();
		/* the list a reload set, or one that could not be listened on, tried again */
		listen_endpoint();
	}
}
