/* SPDX-License-Identifier: Apache-2.0 */
/*-------------------------------------------------------------------------
 *
 * call.c
 *	  One call of the engine: the host it is given (KbHost), the errors its
 *	  callbacks keep, the cursors they open, the output (docs/design.md
 *	  §2.2).
 *
 * The engine's frames are Rust's: a longjmp must never cross them.  So
 * every callback runs its PostgreSQL code under kb_call_guard(), which
 * catches an error, copies it into the caller's memory, flushes it, marks
 * the call failed and returns KB_HOST_ERROR.  The engine returns from its
 * entry point with KB_ERR_HOST, unwinding only its own frames, and
 * kb_call_check() re-throws the kept error, whose abort then releases what
 * the failed callback held -- buffer pins, locks, scans -- through the
 * resource owners, as for any error.  Between the catch and the re-throw
 * the call does no database work: every callback of a failed call fails
 * at once, and a close does nothing.
 *
 * A call that may write (an update, a load) reads its own writes: it reads
 * through a copy of the active snapshot, whose command id it advances
 * before a read that follows a write (kb_call_advance()), as SPI's
 * non-read-only execution does between statements.
 *
 *-------------------------------------------------------------------------
 */
#include "postgres.h"

#include "access/xact.h"
#include "miscadmin.h"
#include "utils/memutils.h"
#include "utils/snapmgr.h"

#include "pg_knowledge_base.h"

StaticAssertDecl(sizeof(sig_atomic_t) == sizeof(int32_t),
				 "InterruptPending is read by the engine as an int32_t");

/* Run fn under PG_TRY: KB_HOST_OK, or the error kept and KB_HOST_ERROR. */
int32
kb_call_guard(KbCall *call, void (*fn) (KbCall *call, void *arg), void *arg)
{
	MemoryContext oldcxt = CurrentMemoryContext;
	volatile bool ok = true;

	if (call->failed)
		return KB_HOST_ERROR;
	PG_TRY();
	{
		CHECK_FOR_INTERRUPTS();
		fn(call, arg);
	}
	PG_CATCH();
	{
		MemoryContextSwitchTo(call->caller);
		call->error = CopyErrorData();
		FlushErrorState();
		MemoryContextSwitchTo(oldcxt);
		call->failed = true;
		ok = false;
	}
	PG_END_TRY();
	return ok ? KB_HOST_OK : KB_HOST_ERROR;
}

typedef struct WriteArgs
{
	const char *data;
	size_t		len;
} WriteArgs;

static void
do_write(KbCall *call, void *arg)
{
	WriteArgs  *w = (WriteArgs *) arg;

	if (call->write_fn)
	{
		if (!call->write_fn(call->write_arg, w->data, w->len))
			ereport(ERROR,
					(errcode(ERRCODE_CONNECTION_FAILURE),
					 errmsg("the client closed the connection")));
	}
	else if (call->out)
	{
		if ((Size) call->out->len + w->len >= MaxAllocSize - 1)
			ereport(ERROR,
					(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
					 errmsg("the result is larger than a text value holds (1 GB)"),
					 errhint("Read large results with kb.sparql() or through the endpoint.")));
		appendBinaryStringInfo(call->out, w->data, (int) w->len);
	}
	else
		elog(ERROR, "the call offers no output");
}

static int32
call_write(void *ctx, const char *data, size_t len)
{
	WriteArgs	w = {data, len};

	return kb_call_guard((KbCall *) ctx, do_write, &w);
}

typedef struct RespondArgs
{
	int32		status;
	const char *content_type;
	const char *headers;
	int32		rc;
} RespondArgs;

static void
do_respond(KbCall *call, void *arg)
{
	RespondArgs *r = (RespondArgs *) arg;

	if (call->respond_fn == NULL)
		elog(ERROR, "the call answers no HTTP request");
	r->rc = call->respond_fn(call->write_arg, r->status, r->content_type, r->headers);
}

static int32
call_respond(void *ctx, int32 status, const char *content_type, const char *headers)
{
	KbCall	   *call = (KbCall *) ctx;
	RespondArgs r = {status, content_type, headers, KB_HOST_OK};
	int32		rc = kb_call_guard(call, do_respond, &r);

	return rc != KB_HOST_OK ? rc : r.rc;
}

static void
do_nothing(KbCall *call, void *arg)
{
	(void) call;
	(void) arg;
}

static int32
call_check_interrupts(void *ctx)
{
	/* the guard runs CHECK_FOR_INTERRUPTS() */
	return kb_call_guard((KbCall *) ctx, do_nothing, NULL);
}

/* The context of every error of a call, the engine's or PostgreSQL's */
static void
call_context(void *arg)
{
	errcontext("%s", (const char *) arg);
}

/*
 * A call begun: context, if not NULL, is what its errors' CONTEXT line
 * says, and must live until the call ends.
 */
KbCall *
kb_call_begin(bool writes, const char *context)
{
	MemoryContext mcxt;
	KbCall	   *call;
	KbConfig	config;

	/* the engine's settings, as the session has them now */
	config.size = sizeof(config);
	config.memory_limit = (int64) kb_engine_memory_limit * 1024;
	config.term_cache_entries = kb_term_cache_entries;
	config.r2rml_pushdown = kb_r2rml_pushdown;
	config.statistics = kb_statistics;
	config.interrupt_pending = (const volatile int32_t *) &InterruptPending;
	kb_api()->configure(&config);

	mcxt = AllocSetContextCreate(CurrentMemoryContext, "pg_knowledge_base call",
								 ALLOCSET_DEFAULT_SIZES);
	call = MemoryContextAllocZero(mcxt, sizeof(KbCall));

	call->mcxt = mcxt;
	call->caller = CurrentMemoryContext;
	call->host.size = sizeof(KbHost);
	call->host.ctx = call;
	kb_store_host(&call->host);
	kb_sql_host(&call->host);
	call->host.write = call_write;
	call->host.respond = call_respond;
	call->host.check_interrupts = call_check_interrupts;
	call->writes = writes;
	if (context != NULL)
	{
		call->errcb.callback = call_context;
		call->errcb.arg = unconstify(char *, context);
		call->errcb.previous = error_context_stack;
		error_context_stack = &call->errcb;
		call->has_context = true;
	}
	if (!ActiveSnapshotSet())
	{
		PushActiveSnapshot(GetTransactionSnapshot());
		call->pushed_snapshot = true;
	}
	else if (writes)
	{
		PushCopiedSnapshot(GetActiveSnapshot());
		call->pushed_snapshot = true;
	}
	return call;
}

void
kb_call_register(KbCall *call, KbCursor *cursor)
{
	MemoryContext old = MemoryContextSwitchTo(call->mcxt);

	cursor->call = call;
	cursor->closed = false;
	call->cursors = lappend(call->cursors, cursor);
	MemoryContextSwitchTo(old);
}

/*
 * Before a read that follows the call's writes: the snapshot sees them.
 * SPI advanced the command counter after each write.
 */
void
kb_call_advance(KbCall *call)
{
	if (call->dirty && call->pushed_snapshot)
	{
		CommandCounterIncrement();
		UpdateActiveSnapshotCommandId();
	}
	call->dirty = false;
}

/* What a successful call leaves: its cursors closed, its snapshot popped. */
static void
call_release(KbCall *call)
{
	ListCell   *lc;

	foreach(lc, call->cursors)
	{
		KbCursor   *cursor = (KbCursor *) lfirst(lc);

		if (!cursor->closed)
		{
			cursor->closed = true;
			cursor->close(cursor);
		}
	}
	call->cursors = NIL;
	kb_sql_close(call);
	kb_store_close(call);
	if (call->pushed_snapshot)
	{
		PopActiveSnapshot();
		call->pushed_snapshot = false;
	}
}

void
kb_call_finish(KbCall *call)
{
	if (call->failed)
		ReThrowError(call->error);
	call_release(call);
	if (call->has_context)
	{
		Assert(error_context_stack == &call->errcb);
		error_context_stack = call->errcb.previous;
	}
	MemoryContextDelete(call->mcxt);
}

/*
 * A call left by an error of the caller's own, not the engine's: no
 * database work, only the call's memory; the abort releases the rest.
 */
void
kb_call_abort(KbCall *call)
{
	call->failed = true;
}

/*
 * An entry point's status: on KB_ERR_HOST the kept error re-thrown, on any
 * other failure the call released and the engine's error raised.
 */
void
kb_call_check(KbCall *call, int32 rc, KbError *err)
{
	if (rc == KB_OK)
		return;
	if (call->failed)
	{
		if (err->message)
			kb_api()->free_string(err->message);
		ReThrowError(call->error);
	}
	if (rc == KB_ERR_HOST)
		elog(ERROR, "the knowledge base engine reported a host error the call did not keep");
	call_release(call);
	kb_raise(err, NULL);			/* in the call's context */
}
