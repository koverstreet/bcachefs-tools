#ifndef __LINUX_CPUMASK_H
#define __LINUX_CPUMASK_H

/*
 * "cpus" in the userspace shim are thread slots in the bch_percpu chunk
 * registry — see linux/percpu.c. Each thread bch_percpu_thread_init()s
 * itself a slot at thread create; cpu N corresponds to bch_percpu_chunks[N].
 * Walking for_each_possible_cpu() over [0, bch_percpu_nr_cpus) covers
 * every thread that has a chunk, so summing percpu counters and the like
 * see every thread's contribution.
 */
extern int bch_percpu_nr_cpus;

#define num_online_cpus()	((unsigned) bch_percpu_nr_cpus)
#define num_possible_cpus()	((unsigned) bch_percpu_nr_cpus)
#define num_present_cpus()	((unsigned) bch_percpu_nr_cpus)
#define num_active_cpus()	((unsigned) bch_percpu_nr_cpus)
#define cpu_online(cpu)		((cpu) < bch_percpu_nr_cpus)
#define cpu_possible(cpu)	((cpu) < bch_percpu_nr_cpus)
#define cpu_present(cpu)	((cpu) < bch_percpu_nr_cpus)
#define cpu_active(cpu)		((cpu) < bch_percpu_nr_cpus)

#define raw_smp_processor_id()	0U

#define for_each_cpu(cpu, mask)			\
	for ((cpu) = 0; (cpu) < bch_percpu_nr_cpus; (cpu)++, (void)mask)
#define for_each_cpu_not(cpu, mask)		\
	for ((cpu) = 0; (cpu) < bch_percpu_nr_cpus; (cpu)++, (void)mask)
#define for_each_cpu_and(cpu, mask, and)	\
	for ((cpu) = 0; (cpu) < bch_percpu_nr_cpus; (cpu)++, (void)mask, (void)and)

#define for_each_possible_cpu(cpu) for_each_cpu((cpu), 1)
#define for_each_online_cpu(cpu)   for_each_cpu((cpu), 1)
#define for_each_present_cpu(cpu)  for_each_cpu((cpu), 1)

#endif /* __LINUX_CPUMASK_H */
