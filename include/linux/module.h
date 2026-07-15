#ifndef _LINUX_MODULE_H
#define _LINUX_MODULE_H

#include <linux/kernel.h>
#include <linux/stat.h>
#include <linux/compiler.h>
#include <linux/export.h>

struct module;

#define module_init(initfn)					\
	__attribute__((constructor(120)))			\
	static void __call_##initfn(void) { BUG_ON(initfn()); }

#if 0
#define module_exit(exitfn)					\
	__attribute__((destructor(109)))			\
	static void __call_##exitfn(void) { exitfn(); }
#endif

#define module_exit(exitfn)					\
	__attribute__((unused))					\
	static void __call_##exitfn(void) { exitfn(); }

#define MODULE_INFO(tag, info)
#define MODULE_ALIAS(_alias)
#define MODULE_SOFTDEP(_softdep)
#define MODULE_LICENSE(_license)
#define MODULE_AUTHOR(_author)
#define MODULE_DESCRIPTION(_description)
#define MODULE_VERSION(_version)

static inline void __module_get(struct module *module)
{
}

static inline int try_module_get(struct module *module)
{
	return 1;
}

static inline void module_put(struct module *module)
{
}

#define module_param_named(name, value, type, perm)
#define MODULE_PARM_DESC(_parm, desc)

#define __MODULE_PARM_TYPE(name, _type)
#define module_param_cb(name, ops, arg, perm)

struct kernel_param;

enum {
	KERNEL_PARAM_OPS_FL_NOARG = (1 << 0)
};

struct kernel_param_ops {
	/* How the ops should behave */
	unsigned int flags;
	/* Returns 0, or -errno.  arg is in kp->arg. */
	int (*set)(const char *val, const struct kernel_param *kp);
	/* Returns length written or -errno.  Buffer is 4k (ie. be short!) */
	int (*get)(char *buffer, const struct kernel_param *kp);
	/* Optional function to free kp->arg when module unloaded. */
	void (*free)(void *arg);
};

struct kernel_param {
	const char *name;
	struct module *mod;
	const struct kernel_param_ops *ops;
	const u16 perm;
	s8 level;
	u8 flags;
	union {
		void *arg;
		const struct kparam_string *str;
		const struct kparam_array *arr;
	};
};

int kstrtobool(const char *, bool *);

static inline int param_set_bool(const char *val, const struct kernel_param *kp)
{
	return kstrtobool(val, kp->arg);
}

#endif /* _LINUX_MODULE_H */
