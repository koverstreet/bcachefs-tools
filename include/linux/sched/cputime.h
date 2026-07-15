
static inline void task_cputime_adjusted(struct task_struct *p, u64 *utime, u64 *stime)
{
	*utime = 0;
	*stime = 0;
}
