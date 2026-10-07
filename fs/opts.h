/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_OPTS_H
#define _BCACHEFS_OPTS_H

#include <linux/bug.h>
#include <linux/log2.h>
#include <linux/sizes.h>
#include <linux/string.h>
#include <linux/sysfs.h>
#include "bcachefs_format.h"
#include "util/darray.h"

#include "opts_gen.h"

extern const char * const bch2_error_actions[];
extern const char * const bch2_degraded_actions[];
extern const char * const bch2_write_degraded_actions[];
extern const char * const bch2_fsck_fix_opts[];
extern const char * const bch2_version_upgrade_opts[];
extern const char * const bch2_sb_features[];
extern const char * const bch2_sb_compat[];
extern const char * const __bch2_btree_ids[];
extern const char * const __bch2_csum_types[];
extern const char * const __bch2_csum_opts[];
extern const char * const __bch2_compression_types[];
extern const char * const bch2_compression_opts[];
extern const char * const __bch2_str_hash_types[];
extern const char * const bch2_str_hash_opts[];
extern const char * const __bch2_data_types[];
extern const char * const bch2_member_states[];
extern const char * const __bch2_reconcile_accounting_types[];
extern const char * const bch2_d_types[];
extern const char * const bch2_scrub_journal_opts[];

void bch2_prt_jset_entry_type(struct printbuf *,	enum bch_jset_entry_type);
void bch2_prt_fs_usage_type(struct printbuf *,		enum bch_fs_usage_type);
void bch2_prt_data_type(struct printbuf *,		enum bch_data_type);
void bch2_prt_csum_opt(struct printbuf *,		enum bch_csum_opt);
void bch2_prt_csum_type(struct printbuf *,		enum bch_csum_type);
void bch2_prt_compression_type(struct printbuf *,	enum bch_compression_type);
void bch2_prt_str_hash_type(struct printbuf *,		enum bch_str_hash_type);
void bch2_prt_reconcile_accounting_type(struct printbuf *, enum bch_reconcile_accounting_type);
void bch2_prt_key_type_error_reason(struct printbuf *,	enum bch_key_type_errors);

static inline const char *bch2_d_type_str(unsigned d_type)
{
	return (d_type < BCH_DT_MAX ? bch2_d_types[d_type] : NULL) ?: "(bad d_type)";
}

/**
 * x(name, shortopt, type, in mem type, mode, sb_opt)
 *
 * @name	- name of mount option, sysfs attribute, and struct bch_opts
 *		  member
 *
 * @mode	- when opt may be set
 *
 * @sb_option	- name of corresponding superblock option
 *
 * @type	- one of OPT_BOOL, OPT_UINT, OPT_STR
 */

/*
 * XXX: add fields for
 *  - default value
 *  - helptext
 */

extern const struct bch_opts bch2_opts_default;

#define opt_defined(_opts, _name)	((_opts)._name##_defined)

#define opt_get(_opts, _name)						\
	(opt_defined(_opts, _name) ? (_opts)._name : bch2_opts_default._name)

#define opt_set(_opts, _name, _v)					\
do {									\
	(_opts)._name##_defined = true;					\
	(_opts)._name = _v;						\
} while (0)

static inline struct bch_opts bch2_opts_empty(void)
{
	return (struct bch_opts) { 0 };
}

void bch2_opts_apply(struct bch_opts *, struct bch_opts);


extern const struct bch_option bch2_opt_table[];

bool bch2_opt_defined_by_id(const struct bch_opts *, enum bch_opt_id);
u64 bch2_opt_get_by_id(const struct bch_opts *, enum bch_opt_id);
void bch2_opt_set_by_id(struct bch_opts *, enum bch_opt_id, u64);

u64 bch2_opt_from_sb(struct bch_sb *, enum bch_opt_id, int);
int bch2_opts_from_sb(struct bch_opts *, struct bch_sb *);
bool __bch2_opt_set_sb(struct bch_sb *, int, const struct bch_option *, u64, const char *);

bool bch2_opt_set_sb(struct bch_fs *, struct bch_dev *, const struct bch_option *, u64, const char *);

int bch2_opt_lookup(const char *);
int bch2_opt_validate(const struct bch_option *, u64, struct printbuf *);
int bch2_opt_parse(struct bch_fs *, const struct bch_option *,
		   const char *, u64 *, struct printbuf *);

#define OPT_SHOW_FULL_LIST	(1 << 0)
#define OPT_SHOW_MOUNT_STYLE	(1 << 1)

void bch2_opt_to_text(struct printbuf *, struct bch_fs *, struct bch_sb *,
		      const struct bch_option *, u64, unsigned);
void bch2_opts_to_text(struct printbuf *,
		       struct bch_opts,
		       struct bch_fs *, struct bch_sb *,
		       struct bch_opts_mask *,
		       unsigned, unsigned, unsigned);

int bch2_opt_hook_pre_set(struct bch_fs *, struct bch_dev *, u64, enum bch_opt_id, u64, bool,
			  struct opt_change_scope *);
int bch2_opts_hooks_pre_set(struct bch_fs *);
void bch2_opt_hook_post_set(struct bch_fs *, struct bch_dev *, u64, enum bch_opt_id, u64);

int bch2_parse_one_mount_opt(struct bch_fs *, struct bch_opts *,
			     struct printbuf *, const char *, const char *,
			     struct printbuf *);
int bch2_parse_mount_opts(struct bch_fs *, struct bch_opts *, struct printbuf *,
			  char *, bool);

static inline void bch2_io_opts_fixups(struct bch_inode_opts *opts)
{
	if (!opts->background_target)
		opts->background_target = opts->foreground_target;
	if (!opts->background_compression)
		opts->background_compression = opts->compression;
	if (opts->data_replicas == 1)
		opts->erasure_code = 0;
	if (opts->nocow) {
		opts->compression = opts->background_compression = 0;
		opts->data_checksum = 0;
		opts->erasure_code = 0;
	}
	/* We currently only support up to RAID6: */
	if (opts->erasure_code)
		opts->data_replicas = min(opts->data_replicas, 3);
}

void bch2_inode_opts_get(struct bch_fs *, struct bch_inode_opts *, bool);
bool bch2_opt_is_inode_opt(enum bch_opt_id);
void bch2_inode_opts_to_text(struct printbuf *, struct bch_fs *, struct bch_inode_opts);

void bch2_opt_change_unlock(struct bch_fs *);
void bch2_opt_change_lock(struct bch_fs *);

DEFINE_GUARD(opt_change_lock, struct bch_fs *,
	     bch2_opt_change_lock(_T),
	     bch2_opt_change_unlock(_T))

#endif /* _BCACHEFS_OPTS_H */
