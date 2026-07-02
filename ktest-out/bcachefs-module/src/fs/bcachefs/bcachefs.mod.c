#include <linux/module.h>
#include <linux/export-internal.h>
#include <linux/compiler.h>

MODULE_INFO(name, KBUILD_MODNAME);

__visible struct module __this_module
__section(".gnu.linkonce.this_module") = {
	.name = KBUILD_MODNAME,
	.init = init_module,
#ifdef CONFIG_MODULE_UNLOAD
	.exit = cleanup_module,
#endif
	.arch = MODULE_ARCH_INIT,
};

KSYMTAB_FUNC(mean_and_variance_get_mean, "");
SYMBOL_FLAGS(mean_and_variance_get_mean, 0x01);
KSYMTAB_FUNC(mean_and_variance_get_mad, "");
SYMBOL_FLAGS(mean_and_variance_get_mad, 0x01);
KSYMTAB_FUNC(mean_and_variance_get_stddev, "");
SYMBOL_FLAGS(mean_and_variance_get_stddev, 0x01);
KSYMTAB_FUNC(mean_and_variance_get_median, "");
SYMBOL_FLAGS(mean_and_variance_get_median, 0x01);
KSYMTAB_FUNC(six_trylock_ip, "");
SYMBOL_FLAGS(six_trylock_ip, 0x01);
KSYMTAB_FUNC(six_relock_ip, "");
SYMBOL_FLAGS(six_relock_ip, 0x01);
KSYMTAB_FUNC(six_lock_ip_waiter, "");
SYMBOL_FLAGS(six_lock_ip_waiter, 0x01);
KSYMTAB_FUNC(six_lock_contended, "");
SYMBOL_FLAGS(six_lock_contended, 0x01);
KSYMTAB_FUNC(six_unlock_ip, "");
SYMBOL_FLAGS(six_unlock_ip, 0x01);
KSYMTAB_FUNC(six_lock_downgrade, "");
SYMBOL_FLAGS(six_lock_downgrade, 0x01);
KSYMTAB_FUNC(six_lock_tryupgrade, "");
SYMBOL_FLAGS(six_lock_tryupgrade, 0x01);
KSYMTAB_FUNC(six_trylock_convert, "");
SYMBOL_FLAGS(six_trylock_convert, 0x01);
KSYMTAB_FUNC(six_lock_increment, "");
SYMBOL_FLAGS(six_lock_increment, 0x01);
KSYMTAB_FUNC(six_lock_wakeup_all, "");
SYMBOL_FLAGS(six_lock_wakeup_all, 0x01);
KSYMTAB_FUNC(six_lock_counts, "");
SYMBOL_FLAGS(six_lock_counts, 0x01);
KSYMTAB_FUNC(six_lock_readers_add, "");
SYMBOL_FLAGS(six_lock_readers_add, 0x01);
KSYMTAB_FUNC(six_lock_exit, "");
SYMBOL_FLAGS(six_lock_exit, 0x01);
KSYMTAB_FUNC(__six_lock_init, "");
SYMBOL_FLAGS(__six_lock_init, 0x01);
KSYMTAB_FUNC(bch2_run_thread_with_stdout, "");
SYMBOL_FLAGS(bch2_run_thread_with_stdout, 0x01);

MODULE_INFO(depends, "");


MODULE_INFO(srcversion, "6BB03CE8212A29783EDB206");
