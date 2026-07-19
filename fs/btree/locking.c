// SPDX-License-Identifier: GPL-2.0

/* DOC_LATEX(btree-locking)
 *
 * Bcachefs uses SIX locks (shared, intent, exclusive) for btree nodes rather
 * than traditional read/write locks. The three states are:
 *
 * \begin{itemize}
 * \item \textbf{Shared}: Does not conflict with other shared locks (like a read lock)
 * \item \textbf{Intent}: Conflicts with other intent locks but not shared locks
 * \item \textbf{Exclusive}: Conflicts with everything (like a write lock)
 * \end{itemize}
 *
 * \paragraph{Why intent locks?}
 *
 * With a regular read/write lock, a read lock cannot be upgraded to a write
 * lock---that leads to deadlock when multiple threads with read locks try to
 * upgrade simultaneously. With complicated data structures like btrees, updates
 * often need to hold write locks for exclusion with other updates for much
 * longer than the part where they actually modify data that needs exclusion
 * from readers.
 *
 * Consider a btree node split. The update starts at a leaf node and discovers
 * it needs to split. Before starting the split, it must acquire a write lock
 * on the parent node---primarily to avoid deadlocking with other splits. It
 * needs at least a read lock on the parent to lock the path to the child node,
 * but it cannot upgrade that read lock to a write lock (to update the parent
 * with pointers to the new children) because that would deadlock with threads
 * splitting sibling leaf nodes.
 *
 * Intent locks solve this. When doing a split, we acquire an intent lock on
 * the parent---exclusive locks (for the actual in-memory modification) are
 * only ever held while modifying in-memory btree contents, which is a much
 * shorter duration than the entire split operation (which requires waiting for
 * new nodes to be written to disk). Readers can continue accessing the parent
 * throughout the split; only the final pointer update requires exclusive
 * access.
 *
 * \paragraph{Parent-child ordering}
 *
 * Intent locks with only three states do introduce another potential deadlock:
 *
 * \begin{verbatim}
 *     Thread A                        Thread B
 *     read            | Parent |      intent
 *     intent          | Child  |      intent
 * \end{verbatim}
 *
 * Thread B is splitting the child node: it has allocated new nodes and written
 * them out, and now needs an exclusive lock on the parent to add the new
 * pointers (after which it will free the old child). Thread A just wants to
 * insert into the child---it has a read lock on the parent, has looked up the
 * child node, and is waiting on thread B to get an intent lock on the child.
 *
 * But thread A has blocked thread B from taking its exclusive lock on the
 * parent, and thread B cannot drop its intent lock on the child until after
 * the new nodes are visible and the old child is freed.
 *
 * The solution: we drop read locks on parent nodes \emph{before} taking intent
 * locks on child nodes. This might cause us to race with the node being freed,
 * so after grabbing the intent lock we verify the node is still valid and redo
 * the traversal if necessary.
 *
 * \paragraph{Sequence numbers and optimistic relocking}
 *
 * SIX locks include embedded sequence numbers, incremented when taking and
 * releasing exclusive locks (much like seqlocks). This allows us to
 * aggressively drop locks---we can usually retake the lock by checking the
 * sequence number rather than redoing the full btree traversal. We also use
 * this for \texttt{try\_upgrade()}: if we discover we need an intent lock (e.g.
 * for a split, or because the caller is inserting into a leaf node they did
 * not get an intent lock for), we can often upgrade without unwinding and
 * redoing the traversal.
 *
 * \paragraph{Cycle detection}
 *
 * Bcachefs uses database-style cycle detection to avoid deadlocks entirely.
 * Before a transaction sleeps waiting on a contended lock, it invokes
 * \texttt{bch2\_check\_for\_deadlock()}, which walks the graph of transactions
 * waiting on locks. The algorithm follows the chain of dependencies: for each
 * lock a transaction holds, check if any other transaction is waiting on that
 * lock; if so, recursively check what locks \emph{that} transaction holds, and
 * so on.
 *
 * If the walk returns to the original transaction, a cycle exists. One
 * transaction in the cycle is selected to abort: it releases all its locks and
 * restarts from the beginning. The transaction layer is designed so that all
 * operations are idempotent and can be safely restarted at any point.
 *
 * This approach eliminates deadlocks entirely and keeps worst-case latency
 * bounded, at the cost of requiring restartable transactions. The same
 * restart infrastructure also provides crash resilience: since every operation
 * can be interrupted and restarted, the filesystem is inherently resilient to
 * interruption at any point---including during recovery itself.
 *
 * The cycle detector runs only when a transaction would block, so it adds no
 * overhead to the fast path. When cycles are detected, they are broken
 * immediately rather than timing out, keeping latency predictable.
 */

#include "bcachefs.h"

#include "btree/bbpos.h"
#include "btree/cache.h"
#include "btree/locking.h"
#include "btree/write.h"

#include "sb/counters.h"

static struct lock_class_key bch2_btree_node_lock_key;

DEFINE_PER_CPU(struct lock_graph, bch2_lock_graph);

void bch2_lock_graph_init_one(struct lock_graph *g)
{
	for (unsigned i = 0; i < ARRAY_SIZE(g->g); i++)
		darray_init(&g->g[i].waitlist);
}

void bch2_lock_graph_exit_one(struct lock_graph *g)
{
	for (unsigned i = 0; i < ARRAY_SIZE(g->g); i++)
		darray_exit(&g->g[i].waitlist);
}

int bch2_lock_graph_init(void)
{
	int cpu;

	for_each_possible_cpu(cpu)
		bch2_lock_graph_init_one(per_cpu_ptr(&bch2_lock_graph, cpu));
	return 0;
}

void bch2_lock_graph_exit(void)
{
	int cpu;

	for_each_possible_cpu(cpu)
		bch2_lock_graph_exit_one(per_cpu_ptr(&bch2_lock_graph, cpu));
}

void bch2_btree_lock_init(struct btree_bkey_cached_common *b,
			  enum six_lock_init_flags flags,
			  gfp_t gfp)
{
	__six_lock_init(&b->lock, "b->c.lock", &bch2_btree_node_lock_key, flags, gfp);
	lockdep_set_notrack_class(&b->lock);
}

/* Btree node locking: */

struct six_lock_count bch2_btree_node_lock_counts(struct btree_trans *trans,
						  struct btree_path *skip,
						  struct btree_bkey_cached_common *b,
						  unsigned level)
{
	struct btree_path *path;
	struct six_lock_count ret;
	unsigned i;

	memset(&ret, 0, sizeof(ret));

	if (IS_ERR_OR_NULL(b))
		return ret;

	trans_for_each_path(trans, path, i)
		if (path != skip && &path->l[level].b->c == b) {
			int t = btree_node_locked_type(path, level);

			if (t != BTREE_NODE_UNLOCKED)
				ret.n[t]++;
		}

	return ret;
}

/* unlock */

void bch2_btree_node_unlock_write(struct btree_trans *trans,
			struct btree_path *path, struct btree *b)
{
	bch2_btree_node_unlock_write_inlined(trans, path, b);
}

/* lock */

static noinline void print_cycle(struct printbuf *out, struct lock_graph *g)
{
	struct trans_waiting_for_lock *i;

	prt_printf(out, "Found lock cycle (%u entries):\n", g->nr);

	for (i = g->g; i < g->g + g->nr; i++) {
		struct task_struct *task = READ_ONCE(i->trans->locking_wait.task);
		if (!task)
			continue;

		bch2_btree_trans_to_text(out, i->trans);
		bch2_prt_task_backtrace(out, task, i == g->g ? 5 : 1, GFP_NOWAIT);
	}
}

static noinline void print_chain(struct printbuf *out, struct lock_graph *g)
{
	if (g->printed_chain || g->nr <= 1)
		return;
	g->printed_chain = true;

	struct trans_waiting_for_lock *i;

	for (i = g->g; i != g->g + g->nr; i++) {
		struct task_struct *task = READ_ONCE(i->trans->locking_wait.task);
		if (i != g->g)
			prt_str(out, "<- ");
		prt_printf(out, "%u ", task ? task->pid : 0);
	}
	prt_newline(out);
}

static void lock_graph_pop_all(struct lock_graph *g)
{
	g->nr = 0;
}

static noinline void lock_graph_pop_from(struct lock_graph *g, struct trans_waiting_for_lock *i)
{
	g->nr = i - g->g;
}

static void lock_graph_down(struct lock_graph *g, struct btree_trans *trans)
{
	/*
	 * Field-by-field init rather than aggregate: we keep waitlist_snap's
	 * data/size across walks so any grown heap buffer is reused.
	 */
	struct trans_waiting_for_lock *top = &g->g[g->nr++];

	top->trans			= trans;
	top->node_want			= trans->locking;
	top->lock_want			= READ_ONCE(trans->locking_wait.lock_want);
	top->level			= 0;
	top->path_idx			= 0;
	top->waitlist_idx		= 0;
	top->node_have			= NULL;
	top->waitlist.nr		= 0;

	g->printed_chain = false;

	if (unlikely(top > g->g &&
		     top->node_want != top[-1].node_have))
		--g->nr;
}

/*
 * Revalidate the "who is blocked on whom" chain we've built up in @g before
 * acting on a suspected cycle.
 *
 * Between the time we descended into frame @i and now, @i's trans could have
 * acquired its lock and moved on (possibly blocked waiting on something else
 * entirely). Two staleness checks, per frame:
 *
 *   - @from->trans->locking != @from->node_want
 *     @from's trans is no longer waiting for the node we recorded at descent.
 *
 *   - i[0].node_have != i[1].node_want
 *     The parent frame is no longer looking at the node the child frame was
 *     blocked on - the edge we built between them is stale.
 *
 * Either makes the cycle hypothesis invalid; pop from the stale frame down
 * and let the caller retry.
 */
static bool lock_graph_remove_non_waiters(struct lock_graph *g,
					  struct trans_waiting_for_lock *from)
{
	struct trans_waiting_for_lock *i;

	if (from->trans->locking != from->node_want) {
		lock_graph_pop_from(g, from);
		return true;
	}

	for (i = from ; i + 1 < g->g + g->nr; i++)
		if (i[0].node_have != i[1].node_want) {
			lock_graph_pop_from(g, i + 1);
			return true;
		}

	return false;
}

static void trace_would_deadlock(struct lock_graph *g, struct btree_trans *trans)
{
	event_inc_trace(trans->c, trans_restart_would_deadlock, buf, ({
		guard(printbuf_atomic)(&buf);
		prt_printf(&buf, "%s\n", trans->fn);
		print_cycle(&buf, g);
	}));
}

static void wake_up_trans(struct btree_trans *trans)
{
	if (closure_get_not_zero(&trans->ref)) {
		wake_up_process(trans->locking_wait.task);
		closure_put(&trans->ref);
	}
}

static int abort_lock(struct lock_graph *g, struct trans_waiting_for_lock *i,
		      int err)
{
	if (i == g->g) {
		trace_would_deadlock(g, i->trans);
		return bch2_trans_restart_foreign_task(i->trans,
					BCH_ERR_transaction_restart_would_deadlock,
					_THIS_IP_);
	} else {
		i->trans->lock_must_abort = true;
		wake_up_trans(i->trans);
		return 0;
	}
}

static struct trans_waiting_for_lock *
btree_trans_abort_preference(struct trans_waiting_for_lock *l,
			     struct trans_waiting_for_lock *r)
{
	if (l->trans->lock_may_not_fail !=
	    r->trans->lock_may_not_fail)
		return l->trans->lock_may_not_fail ? r : l;

	return time_after64(l->trans->locking_wait.trans_start_time,
			    r->trans->locking_wait.trans_start_time)
		? l : r;
}

static noinline __noreturn void break_cycle_fail(struct lock_graph *g)
{
	CLASS(printbuf, buf)();
	guard(printbuf_atomic)(&buf);

	prt_printf(&buf, bch2_fmt(g->g->trans->c, "cycle of nofail locks"));

	for (struct trans_waiting_for_lock *i = g->g; i < g->g + g->nr; i++) {
		struct btree_trans *trans = i->trans;

		bch2_btree_trans_to_text(&buf, trans);

		prt_printf(&buf, "backtrace:\n");
		scoped_guard(printbuf_indent, &buf)
			bch2_prt_task_backtrace(&buf, trans->locking_wait.task, 2, GFP_NOWAIT);
		prt_newline(&buf);
	}

	bch2_print_str(g->g->trans->c, KERN_ERR, buf.buf);
	BUG();
}

static noinline int break_cycle(struct lock_graph *g, struct printbuf *cycle,
				struct trans_waiting_for_lock *from,
				int err)
{
	struct trans_waiting_for_lock *i, *abort = NULL;
	int ret;

	if (lock_graph_remove_non_waiters(g, from))
		return 0;

	/* Only checking, for debugfs: */
	if (cycle) {
		print_cycle(cycle, g);
		ret = -1;
	} else {
		for (i = from; i < g->g + g->nr; i++)
			abort = !abort ? i : btree_trans_abort_preference(abort, i);

		if (unlikely(abort->trans->lock_may_not_fail))
			break_cycle_fail(g);

		ret = abort_lock(g, abort, BCH_ERR_transaction_restart_would_deadlock);
	}

	if (ret)
		lock_graph_pop_all(g);
	else
		lock_graph_pop_from(g, abort);
	return ret;
}

noinline __cold
static int lock_graph_recursion_limit(struct lock_graph *g, struct btree_trans *trans,
				      struct printbuf *cycle)
{
	if (!cycle)
		event_inc_trace(trans->c, trans_restart_would_deadlock_recursion_limit, buf, ({
			guard(printbuf_atomic)(&buf);
			prt_str(&buf, trans->fn);
		}));

	struct btree_trans *orig_trans = g->g->trans;

	if (orig_trans->lock_may_not_fail) {
		/* Other threads will have to rerun the cycle detector: */
		for (struct trans_waiting_for_lock *i = g->g + 1; i < g->g + g->nr; i++)
			wake_up_trans(i->trans);
		return 0;
	}

	return break_cycle(g, cycle, g->g, BCH_ERR_transaction_restart_deadlock_recursion_limit);
}

static inline int lock_graph_descend(struct lock_graph *g, struct btree_trans *trans,
				     struct printbuf *cycle)
{
	for (struct trans_waiting_for_lock *i = g->g; i < g->g + g->nr; i++)
		if (i->trans == trans)
			return break_cycle(g, cycle, i, BCH_ERR_transaction_restart_would_deadlock);

	if (unlikely(g->nr == ARRAY_SIZE(g->g)))
		return lock_graph_recursion_limit(g, trans, cycle);

	lock_graph_down(g, trans);
	return 0;
}

static bool lock_type_conflicts(enum six_lock_type t1, enum six_lock_type t2)
{
	return t1 + t2 > 1;
}

noinline __cold
static int waitlist_alloc_failed(struct lock_graph *g, struct printbuf *cycle)
{
	struct bch_fs *c = g->g->trans->c;

	if (cycle)
		return -1;

	event_inc_trace(c, trans_restart_deadlock_waitlist_alloc, buf, ({
		guard(printbuf_atomic)(&buf);
		prt_str(&buf, g->g->trans->fn);
	}));

	return btree_trans_restart(g->g->trans, BCH_ERR_transaction_restart_deadlock_waitlist_alloc);
}

int bch2_check_for_deadlock(struct btree_trans *trans, struct printbuf *cycle)
{
	btree_path_idx_t path_idx;

	EBUG_ON(cycle && !cycle->atomic);

	/* trans->paths is rcu protected vs. freeing */
	guard(rcu)();
	guard(preempt)();

	struct lock_graph *g = this_cpu_ptr(&bch2_lock_graph);
	g->nr = 0;

	if (trans->lock_must_abort && !trans->lock_may_not_fail) {
		if (cycle)
			return -1;

		trace_would_deadlock(g, trans);
		return btree_trans_restart(trans, BCH_ERR_transaction_restart_would_deadlock);
	}

	lock_graph_down(g, trans);
next:
	if (!g->nr)
		return 0;

	struct trans_waiting_for_lock *top = &g->g[g->nr - 1];

	if (top->waitlist_idx < top->waitlist.nr) {
		try(lock_graph_descend(g, top->waitlist.data[top->waitlist_idx++], cycle));

		goto next;
	}

	top->waitlist_idx = top->waitlist.nr = 0;

	struct btree_path *paths = rcu_dereference(top->trans->paths);
	if (!paths)
		goto up;

	unsigned long *paths_allocated = trans_paths_allocated(paths);

	trans_for_each_path_idx_from(paths_allocated, *trans_paths_nr(paths),
				     path_idx, top->path_idx) {
		struct btree_path *path = paths + path_idx;
		if (!path->nodes_locked)
			continue;

		if (path_idx != top->path_idx) {
			top->path_idx		= path_idx;
			top->level		= 0;
		}

		while (top->level < BTREE_MAX_DEPTH) {
			int lock_held = btree_node_locked_type(path, top->level);

			if (lock_held == BTREE_NODE_UNLOCKED) {
				top->level++;
				continue;
			}

			top->node_have = &READ_ONCE(path->l[top->level].b)->c;
			if (unlikely(IS_ERR_OR_NULL(top->node_have))) {
				/*
				 * If we get here, it means we raced with the
				 * other thread updating its btree_path
				 * structures - which means it can't be blocked
				 * waiting on a lock:
				 */
				if (!lock_graph_remove_non_waiters(g, g->g)) {
					/*
					 * If lock_graph_remove_non_waiters()
					 * didn't do anything, it must be
					 * because we're being called by debugfs
					 * checking for lock cycles, which
					 * invokes us on btree_transactions that
					 * aren't actually waiting on anything.
					 * Just bail out:
					 */
					lock_graph_pop_all(g);
				}

				goto next;
			}

			/*
			 * Lockless walk of wait_fifo: we're under guard(rcu).
			 * The fifo is rcu_dereference'd and, on realloc, the old
			 * buffer is freed via kfree_rcu_mightsleep; trans memory
			 * is RCU-deferred in bch2_trans_put; and slots only
			 * transition between NULL and a valid pointer (never torn -
			 * insert publishes .w via smp_store_release). Per-CPU cache
			 * reuse inside a grace period can aim us at a reused trans —
			 * benign, cycles missed this pass are caught next.
			 *
			 * Snapshot the conflicting trans pointers into a per-frame
			 * darray so iteration is stable across concurrent wakeups.
			 * Heap allocation is GFP_NOWAIT|__GFP_NOWARN (can't sleep
			 * under rcu+preempt). If growth past the inline buffer
			 * fails, silently truncating would risk missing a cycle;
			 * bail out with a dedicated restart type + counter so we
			 * can tell if this ever actually fires in the wild.
			 */
			struct six_lock_wait_fifo *wf =
				rcu_dereference(top->node_have->lock.wait_fifo);
			darray_for_each(*wf, i) {
				/*
				 * Acquire pairs with the smp_store_release(&.w)
				 * in six_lock_wait_fifo_insert: orders this slot's
				 * sibling .start_time read and the container_of(.w)
				 * -> trans held-lock reads after we observe .w.
				 */
				struct six_lock_waiter *w = smp_load_acquire(&i->w);
				trans = container_of_or_null(w, struct btree_trans, locking_wait);

				if (trans &&
				    trans != top->trans &&
				    lock_type_conflicts(lock_held, i->start_time & SIX_LOCK_WANT_MASK)) {
					if (unlikely(darray_push_gfp(&top->waitlist, trans,
								     GFP_NOWAIT|__GFP_NOWARN))) {
						return waitlist_alloc_failed(g, cycle);
					}
				}
			}

			top->level++;

			if (top->waitlist_idx < top->waitlist.nr)
				goto next;
		}
	}
up:
	if (cycle)
		print_chain(cycle, g);
	--g->nr;
	goto next;
}

static inline struct btree *locking_node(struct six_lock *lock)
{
	struct btree_bkey_cached_common *b = container_of(lock, struct btree_bkey_cached_common, lock);
	return !b->cached
		? container_of(b, struct btree, c)
		: NULL;
}

static inline bool node_reuse_race(struct btree_trans *trans, struct btree *b)
{
	if (trans->locking_hash_val)
		return trans->locking_hash_val != b->hash_val;
	else if (trans->locking_root_id != -1)
		return bch2_btree_id_root_b(trans->c, trans->locking_root_id) != b;
	else
		return false;
}

int bch2_six_check_for_deadlock(struct six_lock *lock, struct six_lock_waiter *w)
{
	/*
	 * Store->load fence: orders publishing ourselves as a waiter (done by
	 * six_lock_wait_fifo_insert before should_sleep_fn ran) before the two
	 * reads of shared state that follow. Both are the store-buffer litmus
	 * test - {publish self; read shared state} racing the mirror op on
	 * another CPU - and only a full barrier forbids the both-miss outcome.
	 * smp_rmb() is not enough (it orders load->load, but the publish is a
	 * store), and the fence cannot fold into the publish as a release: a
	 * release is one-way, it orders the publish after prior writes but
	 * can't fence it before these later reads. So one full barrier here,
	 * serving both reads:
	 *
	 *  1. The walk below reads every other waiter. Two waiters closing a
	 *     cycle each do {publish; look for the other}; both missing means
	 *     both park with the cycle undetected - and once every participant
	 *     is parked there is no next lock request, hence no next pass to
	 *     catch it ("missed this pass, caught next" does not fire). The
	 *     cycle must be caught on the spot.
	 *
	 *  2. The node-reuse check just below reads b->hash_val, pairing with
	 *     the reclaim side (clear hash_val; smp_mb; six_lock_wakeup_all in
	 *     bch2_btree_node_transition_state_locked). Both missing means we
	 *     read a stale-valid hash_val and park on a reused node while the
	 *     waker misses our just-published entry.
	 *
	 * Per-edge ordering - reading a waiter's held-lock state once we
	 * observe its slot - is the separate smp_store_release(&.w)/
	 * smp_load_acquire(&.w) pair in the wait_fifo, not this barrier.
	 */
	smp_mb();

	/*
	 * The btree node we're about to sleep on may have been reclaimed/reused
	 * since the caller picked the lock — the path's b pointer is still
	 * valid memory, but the identity behind it is gone. Don't sleep on a
	 * phantom; force a restart so the trans re-traverses to the real
	 * current node (or learns there isn't one).
	 *
	 * Only btree nodes need this: interior updates take node locks
	 * off-path (e.g. via btree_node_reclaim's six_trylock_intent), so the
	 * cycle detector can't see the holder. Key cache entries don't have
	 * that pattern — they're always held via a path the detector walks.
	 *
	 * Compare against the hash_val snapshotted at lock-attempt time in
	 * btree_node_lock_nopath. Checking !hash_val alone is insufficient:
	 * the node may already have been freed *and* re-hashed to a different
	 * identity, in which case hash_val is non-zero but ≠ what we wanted.
	 *
	 * The store->load ordering against the reclaim side
	 * (bch2_btree_node_transition_state_locked) is the smp_mb() above,
	 * point 2.
	 */
	struct btree_trans *trans = container_of(w, struct btree_trans, locking_wait);
	struct btree *b = locking_node(lock);
	if (b && node_reuse_race(trans, b))
		return bch_err_throw(trans->c, no_btree_node_reused);

#if defined(__KERNEL__) && !defined(CONFIG_SCHED_ALT)
	/*
	 * Wake-CPU hint, set at the moment of sleep: nudge the scheduler
	 * toward the CPU whose L1/L2 owns this task's shard's btree-node
	 * working set. Soft — sched is free to override under load; writes
	 * nothing when already matched. Placed here (vs. trans_begin)
	 * because select_task_rq_fair() consults wake_cpu only at wakeup,
	 * so the hint has to survive from the schedule() that follows.
	 *
	 * Compiled out under CONFIG_SCHED_ALT (BMQ/PDS): those schedulers
	 * don't have select_task_rq_fair() and drop task_struct.wake_cpu.
	 */
	if (trans->shard_cpu >= 0 &&
	    trans->shard_cpu != raw_smp_processor_id())
		WRITE_ONCE(current->wake_cpu, trans->shard_cpu);
#endif

	return bch2_check_for_deadlock(trans, NULL);
}

/*
 * Lock a btree node if we already have it locked on one of our linked
 * iterators:
 */
static inline bool btree_node_lock_increment(struct btree_trans *trans,
					     struct btree_bkey_cached_common *b,
					     unsigned level,
					     enum btree_node_locked_type want)
{
	struct btree_path *path;
	unsigned i;

	trans_for_each_path(trans, path, i)
		if (&path->l[level].b->c == b &&
		    btree_node_locked_type(path, level) >= want) {
			six_lock_increment(&b->lock, (enum six_lock_type) want);
			return true;
		}

	return false;
}

int bch2_btree_node_lock_slowpath(struct btree_trans *trans,
			struct btree_path *path,
			struct btree_bkey_cached_common *b,
			unsigned level,
			enum six_lock_type type)
{
	if (!btree_node_lock_increment(trans, b, level, (enum btree_node_locked_type) type)) {
#ifdef CONFIG_BCACHEFS_LOCK_TIME_STATS
		u64 contended_start = local_clock();
#endif
		int ret = btree_node_lock_nopath(trans, b, type, false,
						 btree_path_ip_allocated(path), true);
#ifdef CONFIG_BCACHEFS_LOCK_TIME_STATS
		__bch2_time_stats_update(&btree_trans_stats(trans)->lock_wait_times,
					 contended_start, local_clock());
#endif
		if (ret)
			return ret;
	}

	return 0;
}

int bch2_btree_node_lock_write_contended(struct btree_trans *trans, struct btree_path *path,
				 struct btree_bkey_cached_common *b,
				 bool lock_may_not_fail)
{
	trans->locking_hash_val = 0;
	trans->locking_root_id	= -1;

	/*
	 * Must drop our read locks before calling six_lock_write() -
	 * six_unlock() won't do wakeups until the reader count
	 * goes to 0, and it's safe because we have the node intent
	 * locked:
	 */
	int readers = bch2_btree_node_lock_counts(trans, NULL, b, b->level).n[SIX_LOCK_read];
	if (readers)
		six_lock_readers_add(&b->lock, -readers);

	int ret = btree_node_lock_nopath(trans, b, SIX_LOCK_write,
					 lock_may_not_fail, _RET_IP_, !readers);
	if (readers)
		six_lock_readers_add(&b->lock, readers);

	if (ret)
		mark_btree_node_locked_noreset(path, b->level, BTREE_NODE_INTENT_LOCKED);

	return ret;
}

/*
 * Lock @b when the caller doesn't already have a path for it: create a
 * temporary unlocked path, take the lock, then record the lock on the path
 * so the cycle detector can find us as the holder.
 *
 * Caller releases via bch2_btree_node_unlock_with_path().
 *
 * May return a transaction_restart; wrap in lockrestart_do().
 */
int __must_check
bch2_btree_node_lock_with_path(struct btree_trans *trans,
			       struct btree_bkey_cached_common *b,
			       enum six_lock_type type,
			       btree_path_idx_t *path_idx_out)
{
	btree_path_idx_t path_idx = bch2_path_get_unlocked_mut(trans,
				b->btree_id, b->level, btree_node_pos(b), b->cached);

	struct btree_path *path = trans->paths + path_idx;
	/* No key context here — caller already has the b. Skip the hash_val
	 * check; we're acquiring on a node the caller already validated. */
	trans->locking_hash_val = 0;
	trans->locking_root_id	= -1;
	int ret = btree_node_lock(trans, path, b, b->level, type);
	if (ret) {
		bch2_path_put(trans, path_idx, true);
		return ret;
	}

	mark_btree_node_locked(trans, path, b->level,
			       (enum btree_node_locked_type) type);
	path->l[b->level].lock_seq	= six_lock_seq(&b->lock);
	path->l[b->level].b		= (struct btree *) b;

	*path_idx_out = path_idx;
	return 0;
}

/* relock */

static void get_locks_fail_to_text(struct printbuf *out, struct btree_trans *trans,
				   struct btree_path *old_path,
				   struct btree_path *path,
				   struct get_locks_fail *f)
{
	bch2_bpos_to_text(out, path->pos);
	prt_printf(out, " %s l=%u seq=%u node seq=",
		   bch2_btree_id_str(path->btree_id),
		   f->l, path->l[f->l].lock_seq);
	if (IS_ERR_OR_NULL(f->b)) {
		prt_str(out, bch2_err_str(PTR_ERR(f->b)));
	} else {
		prt_printf(out, "%u", f->b->c.lock.seq);

		struct six_lock_count c =
			bch2_btree_node_lock_counts(trans, NULL, &f->b->c, f->l);
		prt_printf(out, " self locked %u.%u.%u", c.n[0], c.n[1], c.n[2]);

		c = six_lock_counts(&f->b->c.lock);
		prt_printf(out, " total locked %u.%u.%u", c.n[0], c.n[1], c.n[2]);
	}

	prt_newline(out);
	bch2_btree_path_to_text(out, trans, path - trans->paths, old_path);
}

static int btree_path_get_locks(struct btree_trans *trans,
				struct btree_path *path,
				bool upgrade,
				struct get_locks_fail *f,
				int restart_err)
{
	unsigned l = path->level;

	do {
		if (!btree_path_node(path, l))
			break;

		if (!(upgrade
		      ? bch2_btree_node_upgrade(trans, path, l)
		      : bch2_btree_node_relock(trans, path, l)))
			goto err;

		l++;
	} while (l < path->locks_want);

	return 0;
err:
	if (f) {
		f->l	= l;
		f->b	= path->l[l].b;
	}

	/*
	 * Do transaction restart before unlocking, so we don't pop
	 * should_be_locked asserts
	 */
	if (restart_err) {
		btree_trans_restart(trans, restart_err);
	} else if (path->should_be_locked && !trans->restarted) {
		if (upgrade)
			path->locks_want = l;
		return -1;
	}

	__bch2_btree_path_unlock(trans, path);

	/*
	 * When we fail to get a lock, we have to ensure that any child nodes
	 * can't be relocked so bch2_btree_path_traverse has to walk back up to
	 * the node that we failed to relock:
	 */
	do {
		path->l[l].b = upgrade
			? ERR_PTR(-BCH_ERR_no_btree_node_upgrade)
			: ERR_PTR(-BCH_ERR_no_btree_node_relock);
	} while (l--);

	return -restart_err ?: -1;
}

bool __bch2_btree_node_relock(struct btree_trans *trans,
			      struct btree_path *path, unsigned level,
			      bool trace)
{
	struct btree *b = btree_path_node(path, level);
	int want = __btree_lock_want(path, level);

	if (race_fault())
		goto fail;

	if (six_relock_type(&b->c.lock, want, path->l[level].lock_seq) ||
	    (btree_node_lock_seq_matches(path, b, level) &&
	     btree_node_lock_increment(trans, &b->c, level, want))) {
		mark_btree_node_locked(trans, path, level, want);
		return true;
	}
fail:
	if (trace && !trans->notrace_relock_fail)
		event_inc_trace(trans->c, btree_path_relock_fail, buf, ({
			prt_printf(&buf, "%s\n", trans->fn);
			bch2_btree_path_to_text(&buf, trans, path - trans->paths, path);
		}));
	return false;
}

/* upgrade */

bool bch2_btree_node_upgrade(struct btree_trans *trans,
			     struct btree_path *path, unsigned level)
{
	struct btree *b = path->l[level].b;

	if (!is_btree_node(path, level))
		return false;

	switch (btree_lock_want(path, level)) {
	case BTREE_NODE_UNLOCKED:
		EBUG_ON(btree_node_locked(path, level));
		return true;
	case BTREE_NODE_READ_LOCKED:
		EBUG_ON(btree_node_intent_locked(path, level));
		return bch2_btree_node_relock(trans, path, level);
	case BTREE_NODE_INTENT_LOCKED:
		break;
	case BTREE_NODE_WRITE_LOCKED:
		BUG();
	}

	if (btree_node_intent_locked(path, level))
		return true;

	if (race_fault())
		return false;

	if (btree_node_locked(path, level)
	    ? six_lock_tryupgrade(&b->c.lock)
	    : six_relock_type(&b->c.lock, SIX_LOCK_intent, path->l[level].lock_seq))
		goto success;

	if (btree_node_lock_seq_matches(path, b, level) &&
	    btree_node_lock_increment(trans, &b->c, level, BTREE_NODE_INTENT_LOCKED)) {
		btree_node_unlock(trans, path, level);
		goto success;
	}

	event_inc_trace(trans->c, btree_path_upgrade_fail, buf, ({
		prt_printf(&buf, "%s\n", trans->fn);
		bch2_btree_path_to_text(&buf, trans, path - trans->paths, path);
	}));
	return false;
success:
	mark_btree_node_locked_noreset(path, level, BTREE_NODE_INTENT_LOCKED);
	return true;
}

/* Btree path locking: */

__flatten
bool bch2_btree_path_relock_norestart(struct btree_trans *trans, struct btree_path *path)
{
	bool ret = !btree_path_get_locks(trans, path, false, NULL, 0);
	bch2_trans_verify_locks(trans);
	return ret;
}

noinline __cold
static int bch2_btree_path_relock_trace(struct btree_trans *trans, struct btree_path *path)
{
	struct get_locks_fail f;
	struct btree_path old_path = *path;
	int ret = 0;

	if (btree_path_get_locks(trans, path, false, &f, 0)) {
		event_inc_trace(trans->c, trans_restart_relock_path, buf, ({
			prt_printf(&buf, "%s\n", trans->fn);
			get_locks_fail_to_text(&buf, trans, &old_path, path, &f);
		}));
		ret = btree_trans_restart(trans, BCH_ERR_transaction_restart_relock_path);
	}

	bch2_trans_verify_locks(trans);
	return ret;
}

int __bch2_btree_path_relock(struct btree_trans *trans, struct btree_path *path)
{
	if (unlikely(trace_trans_restart_relock_path_enabled()))
		return bch2_btree_path_relock_trace(trans, path);

	int ret = 0;
	if (btree_path_get_locks(trans, path, false, NULL, 0)) {
		event_inc(trans->c, trans_restart_relock_path);
		ret = btree_trans_restart(trans, BCH_ERR_transaction_restart_relock_path);
	}

	bch2_trans_verify_locks(trans);
	return ret;
}

bool __bch2_btree_path_upgrade_norestart(struct btree_trans *trans,
					 struct btree_path *path,
					 unsigned new_locks_want)
{
	path->locks_want = new_locks_want;

	/*
	 * If we need it locked, we can't touch it. Otherwise, we can return
	 * success - bch2_path_get() will use this path, and it'll just be
	 * retraversed:
	 */
	bool ret = !btree_path_get_locks(trans, path, true, NULL, 0) ||
		!path->should_be_locked;

	bch2_btree_path_verify_locks(trans, path);
	return ret;
}

int __bch2_btree_path_upgrade(struct btree_trans *trans,
			      struct btree_path *path,
			      unsigned new_locks_want)
{
	unsigned old_locks = path->nodes_locked;
	unsigned old_locks_want = path->locks_want;

	path->locks_want = max_t(unsigned, path->locks_want, new_locks_want);

	struct get_locks_fail f = {};
	int ret = btree_path_get_locks(trans, path, true, &f,
				BCH_ERR_transaction_restart_upgrade);
	if (!ret)
		goto out;

	/*
	 * XXX: this is ugly - we'd prefer to not be mucking with other
	 * iterators in the btree_trans here.
	 *
	 * On failure to upgrade the iterator, setting iter->locks_want and
	 * calling get_locks() is sufficient to make bch2_btree_path_traverse()
	 * get the locks we want on transaction restart.
	 *
	 * But if this iterator was a clone, on transaction restart what we did
	 * to this iterator isn't going to be preserved.
	 *
	 * Possibly we could add an iterator field for the parent iterator when
	 * an iterator is a copy - for now, we'll just upgrade any other
	 * iterators with the same btree id.
	 *
	 * The code below used to be needed to ensure ancestor nodes get locked
	 * before interior nodes - now that's handled by
	 * bch2_btree_path_traverse_all().
	 */
	if (!path->cached && !trans->in_traverse_all) {
		struct btree_path *linked;
		unsigned i;

		trans_for_each_path(trans, linked, i)
			if (linked != path &&
			    linked->cached == path->cached &&
			    linked->btree_id == path->btree_id &&
			    linked->locks_want < new_locks_want) {
				linked->locks_want = new_locks_want;
				btree_path_get_locks(trans, linked, true, NULL, 0);
			}
	}

	event_inc_trace(trans->c, trans_restart_upgrade, buf, ({
		prt_printf(&buf, "%s\n", trans->fn);
		prt_printf(&buf, "%s %pS\n", trans->fn, (void *) _RET_IP_);
		bch2_bbpos_to_text(&buf, BBPOS(path->btree_id, path->pos));
		prt_newline(&buf);
		prt_printf(&buf, "locks want %u -> %u level %u\n",
			   old_locks_want, new_locks_want, f.l);
		prt_printf(&buf, "nodes_locked %x -> %x\n",
			   old_locks, path->nodes_locked);
		prt_printf(&buf, "node %s ", IS_ERR(f.b) ? bch2_err_str(PTR_ERR(f.b)) :
			   !f.b ? "(null)" : "(node)");
		prt_printf(&buf, "path seq %u node seq %u",
			   IS_ERR_OR_NULL(f.b) ? 0 : f.b->c.lock.seq,
			   path->l[f.l].lock_seq);

		if (!IS_ERR_OR_NULL(f.b)) {
			struct six_lock_count c = six_lock_counts(&f.b->c.lock);
			prt_printf(&buf, " locked %u.%u.%u", c.n[0], c.n[1], c.n[2]);
		}
		prt_printf(&buf, "\npath idx %zu", path - trans->paths);
#ifdef TRACK_PATH_ALLOCATED
		prt_printf(&buf, " allocated: %ps", (void *) path->ip_allocated);
#endif
		prt_newline(&buf);
	}));
out:
	bch2_trans_verify_locks(trans);
	return ret;
}

void __bch2_btree_path_downgrade(struct btree_trans *trans,
				 struct btree_path *path,
				 unsigned new_locks_want)
{
#ifdef CONFIG_BCACHEFS_DEBUG
	unsigned old_locks_want = path->locks_want;
#endif

	if (trans->restarted)
		return;

	EBUG_ON(path->locks_want < new_locks_want);

	path->locks_want = new_locks_want;

	unsigned l;
	while (path->nodes_locked &&
	       (l = btree_path_highest_level_locked(path)) >= path->locks_want) {
		if (l > path->level) {
			btree_node_unlock(trans, path, l);
		} else {
			if (btree_node_intent_locked(path, l)) {
				six_lock_downgrade(&path->l[l].b->c.lock);
				mark_btree_node_locked_noreset(path, l, BTREE_NODE_READ_LOCKED);
			}
			break;
		}
	}

	bch2_btree_path_verify_locks(trans, path);
#ifdef CONFIG_BCACHEFS_DEBUG
	event_trace(trans->c, path_downgrade, buf, ({
		prt_printf(&buf, "%s\n", trans->fn);
		prt_printf(&buf, "old locks_want: %u\n", old_locks_want);
		bch2_btree_path_to_text(&buf, trans, path - trans->paths, path);
	}));
#endif
}

/* Btree transaction locking: */

void bch2_trans_downgrade(struct btree_trans *trans)
{
	struct btree_path *path;
	unsigned i;

	if (trans->restarted)
		return;

	trans_for_each_path(trans, path, i)
		if (path->ref)
			bch2_btree_path_downgrade(trans, path);
}

static inline void __bch2_trans_unlock(struct btree_trans *trans)
{
	struct btree_path *path;
	unsigned i;

	trans_for_each_path(trans, path, i)
		__bch2_btree_path_unlock(trans, path);

	/*
	 * All locks dropped: submit any btree node writes queued in this
	 * trans's context.
	 */
	if (unlikely(trans->queued_write_bios))
		bch2_trans_submit_write_bios(trans);
}

noinline __cold
static int bch2_trans_relock_trace(struct btree_trans *trans)
{
	struct btree_path *path;
	unsigned i;

	trans_for_each_path(trans, path, i) {
		if (!path->should_be_locked)
			continue;

		struct get_locks_fail f;
		struct btree_path old_path = *path;
		int ret = btree_path_get_locks(trans, path, false, &f,
					       BCH_ERR_transaction_restart_relock);
		if (ret) {
			event_inc_trace(trans->c, trans_restart_relock, buf, ({
				prt_printf(&buf, "%s\n", trans->fn);
				get_locks_fail_to_text(&buf, trans, &old_path, path, &f);
			}));

			__bch2_trans_unlock(trans);
			bch2_trans_verify_locks(trans);
			return ret;
		}
	}

	trans_set_locked(trans, true);
	/* Fresh locked section — re-arm the srcu-held-too-long warning. */
	trans->srcu_io_submitted = false;
	bch2_trans_verify_locks(trans);
	return 0;
}

int __bch2_trans_relock(struct btree_trans *trans, bool trace)
{
	bch2_trans_verify_locks(trans);

	if (unlikely(trans->restarted))
		return -((int) trans->restarted);
	if (unlikely(trace_trans_restart_relock_enabled() && trace))
		return bch2_trans_relock_trace(trans);

	struct btree_path *path;
	unsigned i;

	trans_for_each_path(trans, path, i) {
		if (!path->should_be_locked)
			continue;

		int ret = btree_path_get_locks(trans, path, false, NULL,
					       BCH_ERR_transaction_restart_relock);
		if (ret) {
			if (trace)
				event_inc(trans->c, trans_restart_relock);
			__bch2_trans_unlock(trans);
			bch2_trans_verify_locks(trans);
			return ret;
		}
	}

	trans_set_locked(trans, true);
	/* Fresh locked section — re-arm the srcu-held-too-long warning. */
	trans->srcu_io_submitted = false;
	bch2_trans_verify_locks(trans);
	return 0;
}

int bch2_trans_relock_notrace(struct btree_trans *trans)
{
	return __bch2_trans_relock(trans, false);
}

void bch2_trans_unlock(struct btree_trans *trans)
{
	trans_set_unlocked(trans);

	__bch2_trans_unlock(trans);

	/*
	 * Drop the btree cache cannibalize lock too. Holding it across a
	 * trans_unlock - i.e. across a sleep - is the recipe for a resource
	 * deadlock: cannibalize-holder sleeps waiting on the allocator,
	 * allocator needs to grow the btree cache, growing the cache needs
	 * cannibalize, but we're holding it. Releasing on trans_unlock means
	 * cannibalize is only held over non-sleeping critical sections;
	 * callers that need it after a wake re-acquire normally.
	 */
	if (unlikely(trans->btree_cache_cannibalize_locked))
		bch2_btree_cache_cannibalize_unlock(trans);
}

/*
 * Slow devices legitimately hold the srcu lock across submit_bio() for a long
 * time (e.g. scanning the inodes btree off a slow disk during snapshot
 * deletion), so scale the "held too long" warning past the worst observed
 * device latency rather than spamming the log when the real problem is just
 * slow storage. The lock is held over btree node reads on any online device,
 * so that's the latency we key off.
 */
static unsigned long srcu_hold_warn_thresh(struct bch_fs *c)
{
	return max(bch2_dev_latency_max(c, &c->devs_online, READ) * 2, HZ * 10UL);
}

void bch2_trans_unlock_long(struct btree_trans *trans)
{
	bch2_trans_unlock(trans);
	trans_enable_migrate(trans);

	if (trans->srcu_held) {
		struct bch_fs *c = trans->c;
		struct btree_path *path;
		unsigned i;

		trans_for_each_path(trans, path, i)
			if (path->cached && !btree_node_locked(path, 0))
				path->l[0].b = ERR_PTR(-BCH_ERR_no_btree_node_srcu_reset);

		if (unlikely(trans->srcu_held &&
			     !trans->srcu_io_submitted &&
			     time_after(jiffies, trans->srcu_lock_time + HZ * 10) &&
			     time_after(jiffies, trans->srcu_lock_time +
					srcu_hold_warn_thresh(c)))) {
			CLASS(bch_log_msg_ratelimited, msg)(c);

			prt_printf(&msg.m, "btree trans held srcu lock (delaying memory reclaim) for %lu seconds\n",
				   (jiffies - trans->srcu_lock_time) / HZ);
			bch2_sb_recent_counters_to_text(&msg.m, &trans->c->counters);

			bch2_prt_task_backtrace(&msg.m, current, 1, GFP_KERNEL);
		}

		srcu_read_unlock(&c->btree.trans.barrier, trans->srcu_idx);
		trans->srcu_held = false;
	}
}

void bch2_trans_unlock_write(struct btree_trans *trans)
{
	struct btree_path *path;
	unsigned i;

	trans_for_each_path(trans, path, i)
		for (unsigned l = 0; l < BTREE_MAX_DEPTH; l++)
			if (btree_node_write_locked(path, l))
				bch2_btree_node_unlock_write(trans, path, path->l[l].b);
}

int __bch2_trans_mutex_lock(struct btree_trans *trans,
			    struct mutex *lock)
{
	int ret = drop_locks_do(trans, (mutex_lock(lock), 0));

	if (ret)
		mutex_unlock(lock);
	return ret;
}

/* Debug */

void __bch2_btree_path_verify_locks(struct btree_trans *trans, struct btree_path *path)
{
	if (!path->nodes_locked && btree_path_node(path, path->level)) {
		/*
		 * A path may be uptodate and yet have nothing locked if and only if
		 * there is no node at path->level, which generally means we were
		 * iterating over all nodes and got to the end of the btree
		 */
		BUG_ON(path->should_be_locked && trans->locked && !trans->restarted);
	}

	if (!path->nodes_locked)
		return;

	for (unsigned l = 0; l < BTREE_MAX_DEPTH; l++) {
		int want = btree_lock_want(path, l);
		int have = btree_node_locked_type_nowrite(path, l);

		BUG_ON(!is_btree_node(path, l) && have != BTREE_NODE_UNLOCKED);

		BUG_ON(is_btree_node(path, l) && want != have);

		BUG_ON(btree_node_locked(path, l) &&
		       path->l[l].lock_seq != six_lock_seq(&path->l[l].b->c.lock));
	}
}

static bool bch2_trans_locked(struct btree_trans *trans)
{
	struct btree_path *path;
	unsigned i;

	trans_for_each_path(trans, path, i)
		if (path->nodes_locked)
			return true;
	return false;
}

void __bch2_trans_verify_locks(struct btree_trans *trans)
{
	if (!trans->locked) {
		BUG_ON(bch2_trans_locked(trans));
		return;
	}

	struct btree_path *path;
	unsigned i;

	trans_for_each_path(trans, path, i)
		__bch2_btree_path_verify_locks(trans, path);
}
