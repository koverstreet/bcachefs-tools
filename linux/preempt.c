#include "linux/preempt.h"

/*
 * In the kernel, preempt_disable() pins the task to the current CPU so
 * percpu data accessed via this_cpu_ptr() can be read/written non-atomically
 * without another task on the same CPU racing — the read-modify-write is safe
 * because nobody else can run on that CPU until preempt_enable().
 *
 * Userspace storage is genuinely per-thread (DEFINE_PER_CPU lives in a TLS
 * chunk; alloc_percpu() returns offsets into the same chunk — see
 * linux/percpu.c), so this_cpu_ptr() always resolves to memory only the
 * calling thread owns. There's no other task that could race the RMW;
 * preempt_disable() has nothing to protect, and is a no-op.
 *
 * Cross-thread reads via per_cpu_ptr() were never protected by
 * preempt_disable() in the kernel either — that's the usual percpu
 * eventual-consistency contract — so making this a no-op doesn't introduce
 * new races.
 */
void preempt_disable(void) { }
void preempt_enable(void)  { }
