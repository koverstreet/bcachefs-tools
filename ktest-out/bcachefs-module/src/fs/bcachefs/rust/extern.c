#include "codegen-wrapper.h"

// Static wrappers

void bch2_closure_debug_create__extern(struct closure *cl) { bch2_closure_debug_create(cl); }
void bch2_time_stats_update__extern(struct bch2_time_stats *stats, u64 start) { bch2_time_stats_update(stats, start); }
void bch2_time_stats_quantiles_exit__extern(struct bch2_time_stats_quantiles *statq) { bch2_time_stats_quantiles_exit(statq); }
void bch2_time_stats_quantiles_init__extern(struct bch2_time_stats_quantiles *statq) { bch2_time_stats_quantiles_init(statq); }
void * bch2_kvmalloc_noprof__extern(size_t n, gfp_t flags) { return bch2_kvmalloc_noprof(n, flags); }
struct printbuf_restore printbuf_state_save__extern(struct printbuf *buf) { return printbuf_state_save(buf); }
void printbuf_state_restore__extern(struct printbuf *buf, struct printbuf_restore s) { printbuf_state_restore(buf, s); }
struct printbuf bch2_printbuf_init__extern(void) { return bch2_printbuf_init(); }
unsigned int printbuf_remaining_size__extern(struct printbuf *out) { return printbuf_remaining_size(out); }
unsigned int printbuf_remaining__extern(struct printbuf *out) { return printbuf_remaining(out); }
unsigned int printbuf_written__extern(struct printbuf *out) { return printbuf_written(out); }
void printbuf_nul_terminate_reserved__extern(struct printbuf *out) { printbuf_nul_terminate_reserved(out); }
void printbuf_nul_terminate__extern(struct printbuf *out) { printbuf_nul_terminate(out); }
void printbuf_reset_keep_tabstops__extern(struct printbuf *buf) { printbuf_reset_keep_tabstops(buf); }
void printbuf_reset__extern(struct printbuf *buf) { printbuf_reset(buf); }
void printbuf_atomic_inc__extern(struct printbuf *buf) { printbuf_atomic_inc(buf); }
void printbuf_atomic_dec__extern(struct printbuf *buf) { printbuf_atomic_dec(buf); }
int bch2_strtol_h__extern(const char *cp, long *res) { return bch2_strtol_h(cp, res); }
int bch2_strtoul_h__extern(const char *cp, long *res) { return bch2_strtoul_h(cp, res); }
void bch2_ratelimit_reset__extern(struct bch_ratelimit *d) { bch2_ratelimit_reset(d); }
u64 bch2_local_clock__extern(void) { return bch2_local_clock(); }
void bch2_cond_resched__extern(void) { bch2_cond_resched(); }
void bch2_maybe_corrupt_bio__extern(struct bio *bio, unsigned int ratio) { bch2_maybe_corrupt_bio(bio, ratio); }
bool bch2_csum_type_is_encryption__extern(enum bch_csum_type type) { return bch2_csum_type_is_encryption(type); }
__le64 __bch2_sb_magic__extern(struct bch_sb *sb) { return __bch2_sb_magic(sb); }
bool _bch2_err_matches__extern(int err, int class) { return _bch2_err_matches(err, class); }
long bch2_err_class__extern(long err) { return bch2_err_class(err); }
const char * bch2_d_type_str__extern(unsigned int d_type) { return bch2_d_type_str(d_type); }
struct bch_opts bch2_opts_empty__extern(void) { return bch2_opts_empty(); }
void bch2_io_opts_fixups__extern(struct bch_inode_opts *opts) { bch2_io_opts_fixups(opts); }
struct bkey_s bkey_i_to_s__extern(struct bkey_i *k) { return bkey_i_to_s(k); }
struct bkey_s_c bkey_i_to_s_c__extern(const struct bkey_i *k) { return bkey_i_to_s_c(k); }
struct bkey_i_deleted * bkey_deleted_init__extern(struct bkey_i *_k) { return bkey_deleted_init(_k); }
struct bkey_i_whiteout * bkey_whiteout_init__extern(struct bkey_i *_k) { return bkey_whiteout_init(_k); }
struct bkey_i_error * bkey_error_init__extern(struct bkey_i *_k) { return bkey_error_init(_k); }
struct bkey_i_cookie * bkey_cookie_init__extern(struct bkey_i *_k) { return bkey_cookie_init(_k); }
struct bkey_i_hash_whiteout * bkey_hash_whiteout_init__extern(struct bkey_i *_k) { return bkey_hash_whiteout_init(_k); }
struct bkey_i_btree_ptr * bkey_btree_ptr_init__extern(struct bkey_i *_k) { return bkey_btree_ptr_init(_k); }
struct bkey_i_extent * bkey_extent_init__extern(struct bkey_i *_k) { return bkey_extent_init(_k); }
struct bkey_i_reservation * bkey_reservation_init__extern(struct bkey_i *_k) { return bkey_reservation_init(_k); }
struct bkey_i_inode * bkey_inode_init__extern(struct bkey_i *_k) { return bkey_inode_init(_k); }
struct bkey_i_inode_generation * bkey_inode_generation_init__extern(struct bkey_i *_k) { return bkey_inode_generation_init(_k); }
struct bkey_i_dirent * bkey_dirent_init__extern(struct bkey_i *_k) { return bkey_dirent_init(_k); }
struct bkey_i_xattr * bkey_xattr_init__extern(struct bkey_i *_k) { return bkey_xattr_init(_k); }
struct bkey_i_alloc * bkey_alloc_init__extern(struct bkey_i *_k) { return bkey_alloc_init(_k); }
struct bkey_i_quota * bkey_quota_init__extern(struct bkey_i *_k) { return bkey_quota_init(_k); }
struct bkey_i_stripe * bkey_stripe_init__extern(struct bkey_i *_k) { return bkey_stripe_init(_k); }
struct bkey_i_reflink_p * bkey_reflink_p_init__extern(struct bkey_i *_k) { return bkey_reflink_p_init(_k); }
struct bkey_i_reflink_v * bkey_reflink_v_init__extern(struct bkey_i *_k) { return bkey_reflink_v_init(_k); }
struct bkey_i_inline_data * bkey_inline_data_init__extern(struct bkey_i *_k) { return bkey_inline_data_init(_k); }
struct bkey_i_btree_ptr_v2 * bkey_btree_ptr_v2_init__extern(struct bkey_i *_k) { return bkey_btree_ptr_v2_init(_k); }
struct bkey_i_indirect_inline_data * bkey_indirect_inline_data_init__extern(struct bkey_i *_k) { return bkey_indirect_inline_data_init(_k); }
struct bkey_i_alloc_v2 * bkey_alloc_v2_init__extern(struct bkey_i *_k) { return bkey_alloc_v2_init(_k); }
struct bkey_i_subvolume * bkey_subvolume_init__extern(struct bkey_i *_k) { return bkey_subvolume_init(_k); }
struct bkey_i_snapshot * bkey_snapshot_init__extern(struct bkey_i *_k) { return bkey_snapshot_init(_k); }
struct bkey_i_inode_v2 * bkey_inode_v2_init__extern(struct bkey_i *_k) { return bkey_inode_v2_init(_k); }
struct bkey_i_alloc_v3 * bkey_alloc_v3_init__extern(struct bkey_i *_k) { return bkey_alloc_v3_init(_k); }
struct bkey_i_set * bkey_set_init__extern(struct bkey_i *_k) { return bkey_set_init(_k); }
struct bkey_i_lru * bkey_lru_init__extern(struct bkey_i *_k) { return bkey_lru_init(_k); }
struct bkey_i_alloc_v4 * bkey_alloc_v4_init__extern(struct bkey_i *_k) { return bkey_alloc_v4_init(_k); }
struct bkey_i_backpointer * bkey_backpointer_init__extern(struct bkey_i *_k) { return bkey_backpointer_init(_k); }
struct bkey_i_inode_v3 * bkey_inode_v3_init__extern(struct bkey_i *_k) { return bkey_inode_v3_init(_k); }
struct bkey_i_bucket_gens * bkey_bucket_gens_init__extern(struct bkey_i *_k) { return bkey_bucket_gens_init(_k); }
struct bkey_i_snapshot_tree * bkey_snapshot_tree_init__extern(struct bkey_i *_k) { return bkey_snapshot_tree_init(_k); }
struct bkey_i_logged_op_truncate * bkey_logged_op_truncate_init__extern(struct bkey_i *_k) { return bkey_logged_op_truncate_init(_k); }
struct bkey_i_logged_op_finsert * bkey_logged_op_finsert_init__extern(struct bkey_i *_k) { return bkey_logged_op_finsert_init(_k); }
struct bkey_i_accounting * bkey_accounting_init__extern(struct bkey_i *_k) { return bkey_accounting_init(_k); }
struct bkey_i_inode_alloc_cursor * bkey_inode_alloc_cursor_init__extern(struct bkey_i *_k) { return bkey_inode_alloc_cursor_init(_k); }
struct bkey_i_extent_whiteout * bkey_extent_whiteout_init__extern(struct bkey_i *_k) { return bkey_extent_whiteout_init(_k); }
struct bkey_i_logged_op_stripe_update * bkey_logged_op_stripe_update_init__extern(struct bkey_i *_k) { return bkey_logged_op_stripe_update_init(_k); }
struct btree_path * btree_iter_path__extern(struct btree_trans *trans, struct btree_iter *iter) { return btree_iter_path(trans, iter); }
bool bch2_ro_ref_tryget__extern(struct bch_fs *c) { return bch2_ro_ref_tryget(c); }
void bch2_ro_ref_put__extern(struct bch_fs *c) { bch2_ro_ref_put(c); }
unsigned int block_bytes__extern(const struct bch_fs *c) { return block_bytes(c); }
struct timespec64 bch2_time_to_timespec__extern(const struct bch_fs *c, s64 time) { return bch2_time_to_timespec(c, time); }
s64 timespec_to_bch2_time__extern(const struct bch_fs *c, struct timespec64 ts) { return timespec_to_bch2_time(c, ts); }
s64 bch2_current_time__extern(const struct bch_fs *c) { return bch2_current_time(c); }
u64 bch2_current_io_time__extern(const struct bch_fs *c, int rw) { return bch2_current_io_time(c, rw); }
void bch2_set_ra_pages__extern(struct bch_fs *c, unsigned int ra_pages) { bch2_set_ra_pages(c, ra_pages); }
struct stdio_redirect * bch2_fs_stdio_redirect__extern(struct bch_fs *c) { return bch2_fs_stdio_redirect(c); }
bool bch2_discard_opt_enabled__extern(struct bch_fs *c, struct bch_dev *ca) { return bch2_discard_opt_enabled(c, ca); }
int bch2_fs_casefold_enabled__extern(struct bch_fs *c) { return bch2_fs_casefold_enabled(c); }
const char * bch2_fs_name__extern(const struct bch_fs *c) { return bch2_fs_name(c); }
const char * bch2_dev_name__extern(const struct bch_dev *ca) { return bch2_dev_name(ca); }
bool bch2_dev_rotational__extern(struct bch_fs *c, unsigned int dev) { return bch2_dev_rotational(c, dev); }
void bch2_log_msg_start__extern(struct bch_fs *c, struct printbuf *out) { bch2_log_msg_start(c, out); }
void bch2_log_msg_exit__extern(struct bch_log_msg *msg) { bch2_log_msg_exit(msg); }
struct bch_log_msg bch2_log_msg_init__extern(struct bch_fs *c, unsigned int loglevel, bool suppress, bool atomic) { return bch2_log_msg_init(c, loglevel, suppress, atomic); }
bool bpos_eq__extern(struct bpos l, struct bpos r) { return bpos_eq(l, r); }
bool bpos_lt__extern(struct bpos l, struct bpos r) { return bpos_lt(l, r); }
bool bpos_le__extern(struct bpos l, struct bpos r) { return bpos_le(l, r); }
bool bpos_gt__extern(struct bpos l, struct bpos r) { return bpos_gt(l, r); }
bool bpos_ge__extern(struct bpos l, struct bpos r) { return bpos_ge(l, r); }
int bpos_cmp__extern(struct bpos l, struct bpos r) { return bpos_cmp(l, r); }
struct bpos bpos_min__extern(struct bpos l, struct bpos r) { return bpos_min(l, r); }
struct bpos bpos_max__extern(struct bpos l, struct bpos r) { return bpos_max(l, r); }
struct bpos bpos_successor__extern(struct bpos p) { return bpos_successor(p); }
struct bpos bpos_predecessor__extern(struct bpos p) { return bpos_predecessor(p); }
struct bpos bpos_nosnap_successor__extern(struct bpos p) { return bpos_nosnap_successor(p); }
struct bpos bpos_nosnap_predecessor__extern(struct bpos p) { return bpos_nosnap_predecessor(p); }
struct bpos bpos_with_snapshot__extern(struct bpos p, u32 snapshot) { return bpos_with_snapshot(p, snapshot); }
int bch2_compile_bkey_format__extern(const struct bkey_format *format, void *out) { return bch2_compile_bkey_format(format, out); }
void bch2_bkey_format_add_key__extern(struct bkey_format_state *s, const struct bkey *k) { bch2_bkey_format_add_key(s, k); }
bool bch2_bkey_format_field_overflows__extern(struct bkey_format *f, unsigned int i) { return bch2_bkey_format_field_overflows(f, i); }
const struct bkey_ops * bch2_bkey_type_ops__extern(enum bch_bkey_type type) { return bch2_bkey_type_ops(type); }
bool bch2_bkey_maybe_mergable__extern(const struct bkey *l, const struct bkey *r) { return bch2_bkey_maybe_mergable(l, r); }
int bch2_key_trigger__extern(struct btree_trans *trans, struct btree_trigger_op op) { return bch2_key_trigger(trans, op); }
int bch2_key_trigger_old__extern(struct btree_trans *trans, enum btree_id btree_id, unsigned int level, struct bkey_s_c old, enum btree_iter_update_trigger_flags flags) { return bch2_key_trigger_old(trans, btree_id, level, old, flags); }
int bch2_key_trigger_new__extern(struct btree_trans *trans, enum btree_id btree_id, unsigned int level, struct bkey_s new, unsigned int new_buf_u64s, enum btree_iter_update_trigger_flags flags) { return bch2_key_trigger_new(trans, btree_id, level, new, new_buf_u64s, flags); }
int bch2_bkey_check_repair__extern(struct btree_trans *trans, struct btree_iter *iter, enum btree_id btree, unsigned int level, struct bkey_s_c k) { return bch2_bkey_check_repair(trans, iter, btree, level, k); }
void bch2_bkey_compat__extern(const struct bch_fs *c, unsigned int level, enum btree_id btree_id, unsigned int version, unsigned int big_endian, int write, struct bkey_format *f, struct bkey_packed *k) { bch2_bkey_compat(c, level, btree_id, version, big_endian, write, f, k); }
void bch2_btree_evicted_size_record__extern(struct bch_fs *c, u64 hash, u16 live_u64s) { bch2_btree_evicted_size_record(c, hash, live_u64s); }
bool bch2_btree_evicted_size_lookup__extern(struct bch_fs *c, u64 hash, u16 *out) { return bch2_btree_evicted_size_lookup(c, hash, out); }
bool bch2_btree_cache_should_throttle__extern(struct bch_fs *c) { return bch2_btree_cache_should_throttle(c); }
void bch2_btree_cache_update_throttle__extern(struct bch_fs *c) { bch2_btree_cache_update_throttle(c); }
struct btree_root * bch2_btree_id_root__extern(struct bch_fs *c, unsigned int id) { return bch2_btree_id_root(c, id); }
unsigned long bch2_btree_root_pack__extern(struct btree *b) { return bch2_btree_root_pack(b); }
struct btree * bch2_btree_root_unpack_b__extern(unsigned long v) { return bch2_btree_root_unpack_b(v); }
unsigned int bch2_btree_root_unpack_level__extern(unsigned long v) { return bch2_btree_root_unpack_level(v); }
unsigned long bch2_btree_id_root_packed__extern(struct bch_fs *c, unsigned int btree_id) { return bch2_btree_id_root_packed(c, btree_id); }
struct btree * bch2_btree_id_root_b__extern(struct bch_fs *c, unsigned int btree_id) { return bch2_btree_id_root_b(c, btree_id); }
void bch2_bset_set_no_aux_tree__extern(struct btree *b, struct bset_tree *t) { bch2_bset_set_no_aux_tree(b, t); }
struct bset_tree * bch2_bkey_to_bset_inlined__extern(struct btree *b, struct bkey_packed *k) { return bch2_bkey_to_bset_inlined(b, k); }
struct bkey_packed * bch2_bkey_prev_all__extern(struct btree *b, struct bset_tree *t, struct bkey_packed *k) { return bch2_bkey_prev_all(b, t, k); }
struct bkey_packed * bch2_bkey_prev__extern(struct btree *b, struct bset_tree *t, struct bkey_packed *k) { return bch2_bkey_prev(b, t, k); }
bool bch2_btree_node_iter_end__extern(struct btree_node_iter *iter) { return bch2_btree_node_iter_end(iter); }
struct bkey_packed * __bch2_btree_node_iter_peek_all__extern(struct btree_node_iter *iter, struct btree *b) { return __bch2_btree_node_iter_peek_all(iter, b); }
struct bkey_packed * bch2_btree_node_iter_peek_all__extern(struct btree_node_iter *iter, struct btree *b) { return bch2_btree_node_iter_peek_all(iter, b); }
struct bkey_packed * bch2_btree_node_iter_peek__extern(struct btree_node_iter *iter, struct btree *b) { return bch2_btree_node_iter_peek(iter, b); }
struct bkey_packed * bch2_btree_node_iter_next_all__extern(struct btree_node_iter *iter, struct btree *b) { return bch2_btree_node_iter_next_all(iter, b); }
void bch2_btree_node_iter_verify__extern(struct btree_node_iter *iter, struct btree *b) { bch2_btree_node_iter_verify(iter, b); }
void bch2_verify_btree_nr_keys__extern(struct btree *b) { bch2_verify_btree_nr_keys(b); }
size_t extent_entry_u64s__extern(const struct bch_fs *c, const union bch_extent_entry *entry) { return extent_entry_u64s(c, entry); }
struct bch_extent_crc_unpacked bch2_extent_crc_unpack__extern(const struct bkey *k, const union bch_extent_crc *crc) { return bch2_extent_crc_unpack(k, crc); }
struct bkey_ptrs_c bch2_bkey_ptrs_c__extern(struct bkey_s_c k) { return bch2_bkey_ptrs_c(k); }
struct bkey_ptrs bch2_bkey_ptrs__extern(struct bkey_s k) { return bch2_bkey_ptrs(k); }
void bch2_io_failures_exit__extern(struct bch_io_failures *f) { bch2_io_failures_exit(f); }
struct bch_devs_list bch2_bkey_devs__extern(const struct bch_fs *c, struct bkey_s_c k) { return bch2_bkey_devs(c, k); }
int bch2_extent_ptr_desired_durability__extern(struct btree_trans *trans, struct extent_ptr_decoded *p) { return bch2_extent_ptr_desired_durability(trans, p); }
int bch2_extent_ptr_durability__extern(struct btree_trans *trans, struct extent_ptr_decoded *p) { return bch2_extent_ptr_durability(trans, p); }
struct bch_extent_ptr * bch2_bkey_has_device__extern(const struct bch_fs *c, struct bkey_s k, unsigned int dev) { return bch2_bkey_has_device(c, k, dev); }
unsigned int bch2_bkey_dev_ptr_bit__extern(struct bch_fs *c, struct bkey_s_c k, unsigned int dev) { return bch2_bkey_dev_ptr_bit(c, k, dev); }
void bch2_bkey_append_ptr__extern(const struct bch_fs *c, struct bkey_i *k, struct bch_extent_ptr ptr) { bch2_bkey_append_ptr(c, k, ptr); }
bool bch2_extent_ptr_eq__extern(struct bch_extent_ptr ptr1, struct bch_extent_ptr ptr2) { return bch2_extent_ptr_eq(ptr1, ptr2); }
enum bch_extent_overlap bch2_extent_overlap__extern(const struct bkey *k, const struct bkey *m) { return bch2_extent_overlap(k, m); }
void bch2_cut_front__extern(const struct bch_fs *c, struct bpos where, struct bkey_i *k) { bch2_cut_front(c, where, k); }
void bch2_cut_back__extern(struct bpos where, struct bkey_i *k) { bch2_cut_back(where, k); }
void bch2_key_resize__extern(struct bkey *k, unsigned int new_size) { bch2_key_resize(k, new_size); }
u64 bch2_bkey_extent_ptrs_flags__extern(struct bkey_ptrs_c ptrs) { return bch2_bkey_extent_ptrs_flags(ptrs); }
u64 bch2_bkey_extent_flags__extern(struct bkey_s_c k) { return bch2_bkey_extent_flags(k); }
struct bch_member * __bch2_members_v2_get_mut__extern(struct bch_sb_field_members_v2 *mi, unsigned int i) { return __bch2_members_v2_get_mut(mi, i); }
struct bch_member bch2_members_v2_get__extern(struct bch_sb_field_members_v2 *mi, int i) { return bch2_members_v2_get(mi, i); }
struct bch_member bch2_members_v1_get__extern(struct bch_sb_field_members_v1 *mi, int i) { return bch2_members_v1_get(mi, i); }
bool bch2_dev_is_online__extern(struct bch_dev *ca) { return bch2_dev_is_online(ca); }
struct bch_dev * bch2_dev_rcu_noerror__extern(struct bch_fs *arg_0, unsigned int arg_1) { return bch2_dev_rcu_noerror(arg_0, arg_1); }
bool bch2_dev_idx_is_online__extern(struct bch_fs *c, unsigned int dev) { return bch2_dev_idx_is_online(c, dev); }
bool bch2_dev_list_has_dev__extern(struct bch_devs_list devs, unsigned int dev) { return bch2_dev_list_has_dev(devs, dev); }
void bch2_dev_list_drop_dev__extern(struct bch_devs_list *devs, unsigned int dev) { bch2_dev_list_drop_dev(devs, dev); }
void bch2_dev_list_add_dev__extern(struct bch_devs_list *devs, unsigned int dev) { bch2_dev_list_add_dev(devs, dev); }
struct bch_devs_list bch2_dev_list_single__extern(unsigned int dev) { return bch2_dev_list_single(dev); }
struct bch_dev * __bch2_next_dev_idx__extern(struct bch_fs *c, unsigned int idx, const struct bch_devs_mask *mask) { return __bch2_next_dev_idx(c, idx, mask); }
struct bch_dev * __bch2_next_dev__extern(struct bch_fs *c, struct bch_dev *ca, const struct bch_devs_mask *mask) { return __bch2_next_dev(c, ca, mask); }
void bch2_dev_get__extern(struct bch_dev *ca) { bch2_dev_get(ca); }
void __bch2_dev_put__extern(struct bch_dev *ca) { __bch2_dev_put(ca); }
void bch2_dev_put__extern(struct bch_dev *ca) { bch2_dev_put(ca); }
void __free_bch2_dev_put__extern(void *p) { __free_bch2_dev_put(p); }
void bch2_dev_get_outer__extern(struct bch_dev *ca) { bch2_dev_get_outer(ca); }
void bch2_dev_put_outer__extern(struct bch_dev *ca) { bch2_dev_put_outer(ca); }
struct bch_dev * bch2_get_next_dev__extern(struct bch_fs *c, struct bch_dev *ca) { return bch2_get_next_dev(c, ca); }
struct bch_dev * bch2_get_next_online_dev__extern(struct bch_fs *c, struct bch_dev *ca, unsigned int state_mask, int rw, unsigned int ref_idx) { return bch2_get_next_online_dev(c, ca, state_mask, rw, ref_idx); }
bool bch2_dev_exists__extern(const struct bch_fs *c, unsigned int dev) { return bch2_dev_exists(c, dev); }
struct bch_dev * bch2_dev_have_ref__extern(const struct bch_fs *c, unsigned int dev) { return bch2_dev_have_ref(c, dev); }
struct bch_dev * bch2_dev_locked__extern(struct bch_fs *c, unsigned int dev) { return bch2_dev_locked(c, dev); }
bool bch2_dev_bad_or_evacuating_rcu__extern(struct bch_fs *c, unsigned int dev) { return bch2_dev_bad_or_evacuating_rcu(c, dev); }
bool bch2_dev_bad_or_evacuating__extern(struct bch_fs *c, unsigned int dev) { return bch2_dev_bad_or_evacuating(c, dev); }
struct bch_dev * bch2_dev_rcu__extern(struct bch_fs *c, unsigned int dev) { return bch2_dev_rcu(c, dev); }
struct bch_dev * bch2_dev_tryget_noerror__extern(struct bch_fs *c, unsigned int dev) { return bch2_dev_tryget_noerror(c, dev); }
void class_bch2_dev_tryget_noerror_destructor__extern(struct bch_dev **p) { class_bch2_dev_tryget_noerror_destructor(p); }
struct bch_dev * class_bch2_dev_tryget_noerror_constructor__extern(struct bch_fs *c, unsigned int dev) { return class_bch2_dev_tryget_noerror_constructor(c, dev); }
struct bch_dev * bch2_dev_tryget__extern(struct bch_fs *c, unsigned int dev) { return bch2_dev_tryget(c, dev); }
struct bch_dev * bch2_dev_bkey_tryget__extern(struct bch_fs *c, struct bkey_s_c k, unsigned int dev) { return bch2_dev_bkey_tryget(c, k, dev); }
void class_bch2_dev_bkey_tryget_destructor__extern(struct bch_dev **p) { class_bch2_dev_bkey_tryget_destructor(p); }
struct bch_dev * class_bch2_dev_bkey_tryget_constructor__extern(struct bch_fs *c, struct bkey_s_c k, unsigned int dev) { return class_bch2_dev_bkey_tryget_constructor(c, k, dev); }
struct bch_dev * bch2_dev_bucket_tryget_noerror__extern(struct bch_fs *c, struct bpos bucket) { return bch2_dev_bucket_tryget_noerror(c, bucket); }
void class_bch2_dev_bucket_tryget_noerror_destructor__extern(struct bch_dev **p) { class_bch2_dev_bucket_tryget_noerror_destructor(p); }
struct bch_dev * class_bch2_dev_bucket_tryget_noerror_constructor__extern(struct bch_fs *c, struct bpos bucket) { return class_bch2_dev_bucket_tryget_noerror_constructor(c, bucket); }
struct bch_dev * bch2_dev_bucket_tryget__extern(struct bch_fs *c, struct bpos bucket) { return bch2_dev_bucket_tryget(c, bucket); }
void class_bch2_dev_bucket_tryget_destructor__extern(struct bch_dev **p) { class_bch2_dev_bucket_tryget_destructor(p); }
struct bch_dev * class_bch2_dev_bucket_tryget_constructor__extern(struct bch_fs *c, struct bpos bucket) { return class_bch2_dev_bucket_tryget_constructor(c, bucket); }
struct bch_dev * bch2_dev_iterate_noerror__extern(struct bch_fs *c, struct bch_dev *ca, unsigned int dev_idx) { return bch2_dev_iterate_noerror(c, ca, dev_idx); }
struct bch_dev * bch2_dev_iterate__extern(struct bch_fs *c, struct bch_dev *ca, unsigned int dev_idx) { return bch2_dev_iterate(c, ca, dev_idx); }
struct bch_dev * bch2_dev_get_ioref__extern(struct bch_fs *c, unsigned int dev, int rw, unsigned int ref_idx) { return bch2_dev_get_ioref(c, dev, rw, ref_idx); }
bool bch2_member_alive__extern(struct bch_member *m) { return bch2_member_alive(m); }
bool bch2_member_exists__extern(struct bch_sb *sb, unsigned int dev) { return bch2_member_exists(sb, dev); }
struct bch_member_cpu bch2_mi_to_cpu__extern(struct bch_member *mi) { return bch2_mi_to_cpu(mi); }
bool __bch2_dev_btree_bitmap_marked_sectors__extern(struct bch_dev *ca, u64 start, unsigned int sectors, bool with_gc) { return __bch2_dev_btree_bitmap_marked_sectors(ca, start, sectors, with_gc); }
bool bch2_dev_btree_bitmap_marked_sectors__extern(struct bch_dev *ca, u64 start, unsigned int sectors) { return bch2_dev_btree_bitmap_marked_sectors(ca, start, sectors); }
bool bch2_dev_btree_bitmap_marked_sectors_any__extern(struct bch_dev *ca, u64 start, unsigned int sectors) { return bch2_dev_btree_bitmap_marked_sectors_any(ca, start, sectors); }
void bch2_prt_member_name__extern(struct printbuf *out, struct bch_fs *c, unsigned int idx) { bch2_prt_member_name(out, c, idx); }
bool bch2_version_compatible__extern(u16 version) { return bch2_version_compatible(version); }
int bch2_request_incompat_feature__extern(struct bch_fs *c, enum bcachefs_metadata_version version) { return bch2_request_incompat_feature(c, version); }
size_t bch2_sb_field_bytes__extern(struct bch_sb_field *f) { return bch2_sb_field_bytes(f); }
__le64 bch2_sb_magic__extern(struct bch_fs *c) { return bch2_sb_magic(c); }
void bch2_check_set_feature__extern(struct bch_fs *c, unsigned int feat) { bch2_check_set_feature(c, feat); }
btree_path_idx_t bch2_btree_path_make_mut__extern(struct btree_trans *trans, btree_path_idx_t path, bool intent, unsigned long ip) { return bch2_btree_path_make_mut(trans, path, intent, ip); }
btree_path_idx_t bch2_btree_path_set_pos__extern(struct btree_trans *trans, btree_path_idx_t path, const struct bpos *new_pos, bool intent, unsigned long ip) { return bch2_btree_path_set_pos(trans, path, new_pos, intent, ip); }
void bch2_trans_verify_not_unlocked_or_in_restart__extern(struct btree_trans *arg_0) { bch2_trans_verify_not_unlocked_or_in_restart(arg_0); }
struct bkey_s_c bch2_btree_path_peek_slot_exact__extern(struct btree_path *path, struct bkey *u) { return bch2_btree_path_peek_slot_exact(path, u); }
int bch2_trans_mutex_lock__extern(struct btree_trans *trans, struct mutex *lock) { return bch2_trans_mutex_lock(trans, lock); }
void bch2_trans_verify_paths__extern(struct btree_trans *trans) { bch2_trans_verify_paths(trans); }
void bch2_assert_pos_locked__extern(struct btree_trans *trans, enum btree_id btree, struct bpos pos) { bch2_assert_pos_locked(trans, btree, pos); }
int bch2_trans_relock__extern(struct btree_trans *trans) { return bch2_trans_relock(trans); }
long bch2_trans_short_wait_budget__extern(struct btree_trans *trans, long timeout) { return bch2_trans_short_wait_budget(trans, timeout); }
void bch2_trans_verify_not_restarted__extern(struct btree_trans *trans, u32 restart_count) { bch2_trans_verify_not_restarted(trans, restart_count); }
void bch2_btree_path_downgrade__extern(struct btree_trans *trans, struct btree_path *path) { bch2_btree_path_downgrade(trans, path); }
struct bkey_s_c bch2_btree_iter_peek__extern(struct btree_iter *iter) { return bch2_btree_iter_peek(iter); }
struct bkey_s_c bch2_btree_iter_peek_prev__extern(struct btree_iter *iter) { return bch2_btree_iter_peek_prev(iter); }
void __bch2_btree_iter_set_pos__extern(struct btree_iter *iter, struct bpos new_pos) { __bch2_btree_iter_set_pos(iter, new_pos); }
void bch2_btree_iter_set_pos__extern(struct btree_iter *iter, struct bpos new_pos) { bch2_btree_iter_set_pos(iter, new_pos); }
void bch2_btree_iter_set_pos_to_extent_start__extern(struct btree_iter *iter) { bch2_btree_iter_set_pos_to_extent_start(iter); }
void bch2_btree_iter_set_snapshot__extern(struct btree_iter *iter, u32 snapshot) { bch2_btree_iter_set_snapshot(iter, snapshot); }
enum btree_iter_update_trigger_flags bch2_btree_iter_flags__extern(struct btree_trans *trans, unsigned int btree_id, unsigned int level, enum btree_iter_update_trigger_flags flags) { return bch2_btree_iter_flags(trans, btree_id, level, flags); }
void bch2_trans_iter_init_common__extern(struct btree_trans *trans, struct btree_iter *iter, enum btree_id btree, struct bpos pos, unsigned int locks_want, unsigned int depth, enum btree_iter_update_trigger_flags flags, unsigned long ip) { bch2_trans_iter_init_common(trans, iter, btree, pos, locks_want, depth, flags, ip); }
void __bch2_trans_iter_init__extern(struct btree_trans *trans, struct btree_iter *iter, enum btree_id btree, struct bpos pos, enum btree_iter_update_trigger_flags flags) { __bch2_trans_iter_init(trans, iter, btree, pos, flags); }
void bch2_trans_iter_init__extern(struct btree_trans *trans, struct btree_iter *iter, enum btree_id btree, struct bpos pos, enum btree_iter_update_trigger_flags flags) { bch2_trans_iter_init(trans, iter, btree, pos, flags); }
void bch2_trans_node_iter_init__extern(struct btree_trans *trans, struct btree_iter *iter, enum btree_id btree, struct bpos pos, unsigned int locks_want, unsigned int depth, enum btree_iter_update_trigger_flags flags) { bch2_trans_node_iter_init(trans, iter, btree, pos, locks_want, depth, flags); }
void bch2_trans_kmalloc_trace__extern(struct btree_trans *trans, size_t size, unsigned long ip) { bch2_trans_kmalloc_trace(trans, size, ip); }
void * bch2_trans_kmalloc_nomemzero_ip__extern(struct btree_trans *trans, size_t size, unsigned long ip) { return bch2_trans_kmalloc_nomemzero_ip(trans, size, ip); }
void * bch2_trans_kmalloc_ip__extern(struct btree_trans *trans, size_t size, unsigned long ip) { return bch2_trans_kmalloc_ip(trans, size, ip); }
void * bch2_trans_kmalloc__extern(struct btree_trans *trans, size_t size) { return bch2_trans_kmalloc(trans, size); }
void * bch2_trans_kmalloc_nomemzero__extern(struct btree_trans *trans, size_t size) { return bch2_trans_kmalloc_nomemzero(trans, size); }
struct bkey_s_c __bch2_bkey_get_typed__extern(struct btree_iter *iter, enum bch_bkey_type type) { return __bch2_bkey_get_typed(iter, type); }
int __bch2_bkey_get_val_typed__extern(struct btree_trans *trans, enum btree_id btree, struct bpos pos, enum btree_iter_update_trigger_flags flags, enum bch_bkey_type type, unsigned int val_size, void *val) { return __bch2_bkey_get_val_typed(trans, btree, pos, flags, type, val_size, val); }
struct bkey_s_c bch2_btree_iter_peek_prev_type__extern(struct btree_iter *iter, enum btree_iter_update_trigger_flags flags) { return bch2_btree_iter_peek_prev_type(iter, flags); }
struct bkey_s_c bch2_btree_iter_peek_type__extern(struct btree_iter *iter, enum btree_iter_update_trigger_flags flags) { return bch2_btree_iter_peek_type(iter, flags); }
struct bkey_s_c bch2_btree_iter_peek_max_type__extern(struct btree_iter *iter, struct bpos end, enum btree_iter_update_trigger_flags flags) { return bch2_btree_iter_peek_max_type(iter, end, flags); }
u64 bch2_inode_shard_idx__extern(struct bch_fs *c) { return bch2_inode_shard_idx(c); }
unsigned int bch2_inode_shard_cpu__extern(struct bch_fs *c) { return bch2_inode_shard_cpu(c); }
int bch2_btree_path_traverse__extern(struct btree_trans *trans, btree_path_idx_t path, enum btree_iter_update_trigger_flags flags) { return bch2_btree_path_traverse(trans, path, flags); }
void __bch2_btree_node_unlock_write__extern(struct btree_trans *trans, struct btree *b) { __bch2_btree_node_unlock_write(trans, b); }
void bch2_btree_node_unlock_write_inlined__extern(struct btree_trans *trans, struct btree_path *path, struct btree *b) { bch2_btree_node_unlock_write_inlined(trans, path, b); }
void __bch2_btree_path_unlock__extern(struct btree_trans *trans, struct btree_path *path) { __bch2_btree_path_unlock(trans, path); }
void bch2_btree_node_unlock_with_path__extern(struct btree_trans *trans, btree_path_idx_t path_idx, unsigned int level) { bch2_btree_node_unlock_with_path(trans, path_idx, level); }
int bch2_btree_node_lock_write__extern(struct btree_trans *trans, struct btree_path *path, struct btree_bkey_cached_common *b) { return bch2_btree_node_lock_write(trans, path, b); }
void bch2_btree_node_lock_write_nofail__extern(struct btree_trans *trans, struct btree_path *path, struct btree_bkey_cached_common *b) { bch2_btree_node_lock_write_nofail(trans, path, b); }
int bch2_btree_path_relock__extern(struct btree_trans *trans, struct btree_path *path) { return bch2_btree_path_relock(trans, path); }
bool bch2_btree_node_relock__extern(struct btree_trans *trans, struct btree_path *path, unsigned int level) { return bch2_btree_node_relock(trans, path, level); }
bool bch2_btree_node_relock_notrace__extern(struct btree_trans *trans, struct btree_path *path, unsigned int level) { return bch2_btree_node_relock_notrace(trans, path, level); }
bool bch2_btree_path_upgrade_norestart__extern(struct btree_trans *trans, struct btree_path *path, unsigned int new_locks_want) { return bch2_btree_path_upgrade_norestart(trans, path, new_locks_want); }
int bch2_btree_path_upgrade__extern(struct btree_trans *trans, struct btree_path *path, unsigned int new_locks_want) { return bch2_btree_path_upgrade(trans, path, new_locks_want); }
void bch2_btree_path_verify_locks__extern(struct btree_trans *trans, struct btree_path *path) { bch2_btree_path_verify_locks(trans, path); }
void bch2_trans_verify_locks__extern(struct btree_trans *trans) { bch2_trans_verify_locks(trans); }
struct jset_entry * bch2_journal_add_entry_noreservation__extern(struct journal_buf *buf, size_t u64s) { return bch2_journal_add_entry_noreservation(buf, u64s); }
struct jset_entry * __bch2_journal_add_entry__extern(struct jset_entry **cur, unsigned int type, enum btree_id id, unsigned int level, unsigned int u64s) { return __bch2_journal_add_entry(cur, type, id, level, u64s); }
struct jset_entry * bch2_journal_add_entry__extern(struct journal *j, struct journal_res *res, unsigned int type, enum btree_id id, unsigned int level, unsigned int u64s) { return bch2_journal_add_entry(j, res, type, id, level, u64s); }
int bch2_journal_error__extern(struct journal *j) { return bch2_journal_error(j); }
void __bch2_journal_buf_put__extern(struct journal *j, u64 seq) { __bch2_journal_buf_put(j, seq); }
void bch2_journal_buf_put__extern(struct journal *j, u64 seq) { bch2_journal_buf_put(j, seq); }
void bch2_journal_res_put__extern(struct journal *j, struct journal_res *res) { bch2_journal_res_put(j, res); }
int bch2_journal_res_get__extern(struct journal *j, struct journal_res *res, unsigned int u64s, unsigned int flags, struct btree_trans *trans) { return bch2_journal_res_get(j, res, u64s, flags, trans); }
void bch2_journal_res_flush__extern(struct journal *j, struct journal_res *res, struct closure *cl) { bch2_journal_res_flush(j, res, cl); }
u32 bch2_snapshot_tree__extern(struct bch_fs *c, u32 id) { return bch2_snapshot_tree(c, id); }
bool bch2_snapshots_same_tree__extern(struct bch_fs *c, u32 id1, u32 id2) { return bch2_snapshots_same_tree(c, id1, id2); }
u32 __bch2_snapshot_parent_early__extern(struct bch_fs *c, u32 id) { return __bch2_snapshot_parent_early(c, id); }
u32 bch2_snapshot_parent_early__extern(struct bch_fs *c, u32 id) { return bch2_snapshot_parent_early(c, id); }
u32 __bch2_snapshot_parent__extern(struct bch_fs *c, struct snapshot_table *t, u32 id) { return __bch2_snapshot_parent(c, t, id); }
u32 bch2_snapshot_parent__extern(struct bch_fs *c, u32 id) { return bch2_snapshot_parent(c, id); }
u32 bch2_snapshot_nth_parent__extern(struct bch_fs *c, u32 id, u32 n) { return bch2_snapshot_nth_parent(c, id, n); }
u32 bch2_snapshot_root__extern(struct bch_fs *c, u32 id) { return bch2_snapshot_root(c, id); }
bool __bch2_snapshot_exists__extern(struct snapshot_table *t, u32 id) { return __bch2_snapshot_exists(t, id); }
bool bch2_snapshot_exists__extern(struct bch_fs *c, u32 id) { return bch2_snapshot_exists(c, id); }
int bch2_snapshot_is_internal_node__extern(struct bch_fs *c, u32 id) { return bch2_snapshot_is_internal_node(c, id); }
int bch2_snapshot_is_leaf__extern(struct bch_fs *c, u32 id) { return bch2_snapshot_is_leaf(c, id); }
u32 bch2_snapshot_depth__extern(struct bch_fs *c, u32 parent) { return bch2_snapshot_depth(c, parent); }
u32 bch2_snapshot_live_descendent__extern(struct bch_fs *c, u32 id) { return bch2_snapshot_live_descendent(c, id); }
bool bch2_snapshot_is_ancestor__extern(struct btree_trans *trans, u32 id, u32 ancestor) { return bch2_snapshot_is_ancestor(trans, id, ancestor); }
bool bch2_snapshot_has_children__extern(struct bch_fs *c, u32 id) { return bch2_snapshot_has_children(c, id); }
int bch2_check_key_has_snapshot__extern(struct btree_trans *trans, struct btree_iter *iter, struct bkey_s_c k) { return bch2_check_key_has_snapshot(trans, iter, k); }
int bch2_get_snapshot_overwrites__extern(struct btree_trans *trans, enum btree_id btree, struct bpos pos, snapshot_id_list *s) { return bch2_get_snapshot_overwrites(trans, btree, pos, s); }
int bch2_key_has_snapshot_overwrites__extern(struct btree_trans *trans, enum btree_id id, struct bpos pos) { return bch2_key_has_snapshot_overwrites(trans, id, pos); }
int bch2_btree_delete_at_buffered__extern(struct btree_trans *trans, enum btree_id btree, struct bpos pos) { return bch2_btree_delete_at_buffered(trans, btree, pos); }
int bch2_insert_snapshot_whiteouts__extern(struct btree_trans *trans, enum btree_id btree, struct bpos old_pos, struct bpos new_pos) { return bch2_insert_snapshot_whiteouts(trans, btree, old_pos, new_pos); }
int bch2_trans_update_buf__extern(struct btree_trans *trans, struct btree_iter *iter, struct bkey_i *k, unsigned int k_buf_u64s, enum btree_iter_update_trigger_flags flags) { return bch2_trans_update_buf(trans, iter, k, k_buf_u64s, flags); }
int bch2_trans_update__extern(struct btree_trans *trans, struct btree_iter *iter, struct bkey_i *k, enum btree_iter_update_trigger_flags flags) { return bch2_trans_update(trans, iter, k, flags); }
void * bch2_trans_subbuf_alloc_ip__extern(struct btree_trans *trans, struct btree_trans_subbuf *buf, unsigned int u64s, ulong ip) { return bch2_trans_subbuf_alloc_ip(trans, buf, u64s, ip); }
void * bch2_trans_subbuf_alloc__extern(struct btree_trans *trans, struct btree_trans_subbuf *buf, unsigned int u64s) { return bch2_trans_subbuf_alloc(trans, buf, u64s); }
struct jset_entry * bch2_trans_jset_entry_alloc_ip__extern(struct btree_trans *trans, unsigned int u64s, ulong ip) { return bch2_trans_jset_entry_alloc_ip(trans, u64s, ip); }
struct jset_entry * bch2_trans_jset_entry_alloc__extern(struct btree_trans *trans, unsigned int u64s) { return bch2_trans_jset_entry_alloc(trans, u64s); }
int bch2_btree_write_buffer_insert_checks__extern(struct bch_fs *c, enum btree_id btree, struct bkey_i *k) { return bch2_btree_write_buffer_insert_checks(c, btree, k); }
int bch2_trans_update_buffered__extern(struct btree_trans *trans, enum btree_id btree, struct bkey_i *k) { return bch2_trans_update_buffered(trans, btree, k); }
bool bch2_trans_has_updates__extern(struct btree_trans *trans) { return bch2_trans_has_updates(trans); }
void bch2_trans_reset_updates__extern(struct btree_trans *trans) { bch2_trans_reset_updates(trans); }
int bch2_trans_commit__extern(struct btree_trans *trans, struct disk_reservation *disk_res, u64 *journal_seq, enum bch_trans_commit_flags flags) { return bch2_trans_commit(trans, disk_res, journal_seq, flags); }
int bch2_trans_commit_flush__extern(struct btree_trans *trans, struct disk_reservation *disk_res, u64 *journal_seq, struct closure *flush, enum bch_trans_commit_flags flags) { return bch2_trans_commit_flush(trans, disk_res, journal_seq, flush, flags); }
int bch2_trans_commit_lazy__extern(struct btree_trans *trans, struct disk_reservation *disk_res, u64 *journal_seq, unsigned int flags) { return bch2_trans_commit_lazy(trans, disk_res, journal_seq, flags); }
struct bkey_i * __bch2_bkey_make_mut_noupdate__extern(struct btree_trans *trans, struct bkey_s_c k, unsigned int type, unsigned int min_bytes) { return __bch2_bkey_make_mut_noupdate(trans, k, type, min_bytes); }
struct bkey_i * bch2_bkey_make_mut_noupdate__extern(struct btree_trans *trans, struct bkey_s_c k) { return bch2_bkey_make_mut_noupdate(trans, k); }
struct bkey_i * __bch2_bkey_make_mut__extern(struct btree_trans *trans, struct btree_iter *iter, struct bkey_s_c *k, enum btree_iter_update_trigger_flags flags, unsigned int type, unsigned int min_bytes) { return __bch2_bkey_make_mut(trans, iter, k, flags, type, min_bytes); }
struct bkey_i * bch2_bkey_make_mut__extern(struct btree_trans *trans, struct btree_iter *iter, struct bkey_s_c *k, enum btree_iter_update_trigger_flags flags) { return bch2_bkey_make_mut(trans, iter, k, flags); }
struct bkey_i * __bch2_bkey_get_mut_noupdate__extern(struct btree_iter *iter, unsigned int type, unsigned int min_bytes) { return __bch2_bkey_get_mut_noupdate(iter, type, min_bytes); }
struct bkey_i * bch2_bkey_get_mut_noupdate__extern(struct btree_iter *iter) { return bch2_bkey_get_mut_noupdate(iter); }
struct bkey_i * __bch2_bkey_get_mut__extern(struct btree_trans *trans, enum btree_id btree, struct bpos pos, enum btree_iter_update_trigger_flags flags, unsigned int type, unsigned int min_bytes) { return __bch2_bkey_get_mut(trans, btree, pos, flags, type, min_bytes); }
struct bkey_i * bch2_bkey_get_mut_minsize__extern(struct btree_trans *trans, unsigned int btree_id, struct bpos pos, enum btree_iter_update_trigger_flags flags, unsigned int min_bytes) { return bch2_bkey_get_mut_minsize(trans, btree_id, pos, flags, min_bytes); }
struct bkey_i * bch2_bkey_get_mut__extern(struct btree_trans *trans, unsigned int btree_id, struct bpos pos, enum btree_iter_update_trigger_flags flags) { return bch2_bkey_get_mut(trans, btree_id, pos, flags); }
struct bkey_i * __bch2_bkey_alloc__extern(struct btree_trans *trans, struct btree_iter *iter, enum btree_iter_update_trigger_flags flags, unsigned int type, unsigned int val_size) { return __bch2_bkey_alloc(trans, iter, flags, type, val_size); }
int bch2_foreground_maybe_merge__extern(struct btree_trans *trans, btree_path_idx_t path_idx, unsigned int level, enum bch_trans_commit_flags flags, int u64s_delta, u64 *merge_count) { return bch2_foreground_maybe_merge(trans, path_idx, level, flags, u64s_delta, merge_count); }
ssize_t __bch2_btree_u64s_remaining__extern(struct btree *b, void *end) { return __bch2_btree_u64s_remaining(b, end); }
size_t bch2_btree_keys_u64s_remaining__extern(struct btree *b) { return bch2_btree_keys_u64s_remaining(b); }
bool bch2_btree_node_insert_fits__extern(struct btree *b, unsigned int u64s) { return bch2_btree_node_insert_fits(b, u64s); }
bool bch2_btree_node_compact_fits__extern(struct bch_fs *c, struct btree *b, unsigned int new_key_u64s) { return bch2_btree_node_compact_fits(c, b, new_key_u64s); }
bool bch2_btree_interior_updates_pending__extern(struct bch_fs *c) { return bch2_btree_interior_updates_pending(c); }
void bch2_u64s_neg__extern(u64 *v, unsigned int nr) { bch2_u64s_neg(v, nr); }
unsigned int bch2_accounting_counters__extern(const struct bkey *k) { return bch2_accounting_counters(k); }
void bch2_accounting_neg__extern(struct bkey_s_accounting a) { bch2_accounting_neg(a); }
bool bch2_accounting_key_is_zero__extern(struct bkey_s_c_accounting a) { return bch2_accounting_key_is_zero(a); }
void bch2_accounting_accumulate__extern(struct bkey_i_accounting *dst, struct bkey_s_c_accounting src) { bch2_accounting_accumulate(dst, src); }
void bch2_accounting_accumulate_maybe_kill__extern(struct bch_fs *c, struct bkey_i_accounting *dst, struct bkey_s_c_accounting src) { bch2_accounting_accumulate_maybe_kill(c, dst, src); }
void bpos_to_disk_accounting_pos__extern(struct disk_accounting_pos *acc, struct bpos p) { bpos_to_disk_accounting_pos(acc, p); }
bool bch2_accounting_is_mem__extern(struct disk_accounting_pos *acc) { return bch2_accounting_is_mem(acc); }
bool bch2_bkey_is_accounting_mem__extern(struct bkey *k) { return bch2_bkey_is_accounting_mem(k); }
int bch2_accounting_mem_mod_locked__extern(struct btree_trans *trans, struct bkey_s_c_accounting a, enum bch_accounting_mode mode, bool write_locked) { return bch2_accounting_mem_mod_locked(trans, a, mode, write_locked); }
int bch2_accounting_mem_add__extern(struct btree_trans *trans, struct bkey_s_c_accounting a, bool gc) { return bch2_accounting_mem_add(trans, a, gc); }
void bch2_accounting_mem_read_counters__extern(struct bch_accounting_mem *acc, unsigned int idx, u64 *v, unsigned int nr, bool gc) { bch2_accounting_mem_read_counters(acc, idx, v, nr, gc); }
void bch2_accounting_mem_read__extern(struct bch_fs *c, struct bpos p, u64 *v, unsigned int nr) { bch2_accounting_mem_read(c, p, v, nr); }
int bch2_accounting_trans_commit_hook__extern(struct btree_trans *trans, struct bkey_i_accounting *a, unsigned int commit_flags) { return bch2_accounting_trans_commit_hook(trans, a, commit_flags); }
void bch2_accounting_trans_commit_revert__extern(struct btree_trans *trans, struct bkey_i_accounting *a_i, unsigned int commit_flags) { bch2_accounting_trans_commit_revert(trans, a_i, commit_flags); }
struct bch_dev_usage bch2_dev_usage_read__extern(struct bch_dev *ca) { return bch2_dev_usage_read(ca); }
struct bch_dev_usage_full bch2_dev_usage_full_read__extern(struct bch_dev *ca) { return bch2_dev_usage_full_read(ca); }
u64 bch2_dev_buckets_reserved__extern(struct bch_dev *ca, enum bch_watermark watermark) { return bch2_dev_buckets_reserved(ca, watermark); }
int bch2_bucket_ref_update__extern(struct btree_trans *trans, struct bch_dev *ca, struct bkey_s_c k, const struct bch_extent_ptr *ptr, s64 sectors, enum bch_data_type ptr_data_type, u8 b_gen, u8 *bucket_data_type, u32 *bucket_sectors) { return bch2_bucket_ref_update(trans, ca, k, ptr, sectors, ptr_data_type, b_gen, bucket_data_type, bucket_sectors); }
const char * bch2_data_type_str__extern(enum bch_data_type type) { return bch2_data_type_str(type); }
void bch2_disk_reservation_put__extern(struct bch_fs *c, struct disk_reservation *res) { bch2_disk_reservation_put(c, res); }
int bch2_disk_reservation_add__extern(struct bch_fs *c, struct disk_reservation *res, u64 sectors, enum bch_reservation_flags flags) { return bch2_disk_reservation_add(c, res, sectors, flags); }
struct disk_reservation bch2_disk_reservation_init__extern(struct bch_fs *c, unsigned int nr_replicas) { return bch2_disk_reservation_init(c, nr_replicas); }
int bch2_disk_reservation_get__extern(struct bch_fs *c, struct disk_reservation *res, u64 sectors, unsigned int nr_replicas, int flags) { return bch2_disk_reservation_get(c, res, sectors, nr_replicas, flags); }
bool bch2_bucket_nouse__extern(struct bch_dev *ca, u64 bucket) { return bch2_bucket_nouse(ca, bucket); }
bool bch2_dev_bucket_exists__extern(struct bch_fs *c, struct bpos pos) { return bch2_dev_bucket_exists(c, pos); }
s64 bch2_bucket_sectors_total__extern(struct bch_alloc_v4 a) { return bch2_bucket_sectors_total(a); }
s64 bch2_bucket_sectors_dirty__extern(struct bch_alloc_v4 a) { return bch2_bucket_sectors_dirty(a); }
s64 bch2_bucket_sectors__extern(struct bch_alloc_v4 a) { return bch2_bucket_sectors(a); }
s64 bch2_bucket_sectors_fragmented__extern(struct bch_dev *ca, struct bch_alloc_v4 a) { return bch2_bucket_sectors_fragmented(ca, a); }
s64 bch2_bucket_sectors_unstriped__extern(struct bch_alloc_v4 a) { return bch2_bucket_sectors_unstriped(a); }
const struct bch_alloc_v4 * bch2_alloc_to_v4__extern(struct bkey_s_c k, struct bch_alloc_v4 *convert) { return bch2_alloc_to_v4(k, convert); }
bool bch2_target_accepts_data__extern(struct bch_fs *c, enum bch_data_type data_type, u16 target) { return bch2_target_accepts_data(c, data_type, target); }
bool bch2_dev_in_target__extern(struct bch_fs *c, unsigned int dev, unsigned int target) { return bch2_dev_in_target(c, dev, target); }
bool bch2_checksum_mergeable__extern(unsigned int type) { return bch2_checksum_mergeable(type); }
void bch2_csum_to_text__extern(struct printbuf *out, enum bch_csum_type type, struct bch_csum csum) { bch2_csum_to_text(out, type, csum); }
void bch2_csum_err_msg__extern(struct printbuf *out, enum bch_csum_type type, struct bch_csum expected, struct bch_csum got) { bch2_csum_err_msg(out, type, expected, got); }
int bch2_encrypt_bio__extern(struct bch_fs *c, unsigned int type, struct nonce nonce, struct bio *bio) { return bch2_encrypt_bio(c, type, nonce, bio); }
enum bch_csum_type bch2_csum_opt_to_type__extern(enum bch_csum_opt type, bool data) { return bch2_csum_opt_to_type(type, data); }
enum bch_csum_type bch2_data_checksum_type__extern(struct bch_fs *c, struct bch_inode_opts opts) { return bch2_data_checksum_type(c, opts); }
enum bch_csum_type bch2_data_checksum_type_rb__extern(struct bch_fs *c, struct bch_extent_reconcile opts) { return bch2_data_checksum_type_rb(c, opts); }
enum bch_csum_type bch2_meta_checksum_type__extern(struct bch_fs *c) { return bch2_meta_checksum_type(c); }
bool bch2_checksum_type_valid__extern(const struct bch_fs *c, unsigned int type) { return bch2_checksum_type_valid(c, type); }
bool bch2_crc_cmp__extern(struct bch_csum l, struct bch_csum r) { return bch2_crc_cmp(l, r); }
bool bch2_key_is_encrypted__extern(struct bch_encrypted_key *key) { return bch2_key_is_encrypted(key); }
struct nonce __bch2_sb_key_nonce__extern(struct bch_sb *sb) { return __bch2_sb_key_nonce(sb); }
struct nonce bch2_sb_key_nonce__extern(struct bch_fs *c) { return bch2_sb_key_nonce(c); }
void bch2_bbpos_to_text__extern(struct printbuf *out, struct bbpos pos) { bch2_bbpos_to_text(out, pos); }
int bch2_bkey_buf_realloc_noprof__extern(struct bkey_buf *s, unsigned int u64s) { return bch2_bkey_buf_realloc_noprof(s, u64s); }
int bch2_bkey_buf_reassemble_noprof__extern(struct bkey_buf *s, struct bkey_s_c k) { return bch2_bkey_buf_reassemble_noprof(s, k); }
int bch2_bkey_buf_copy_noprof__extern(struct bkey_buf *s, struct bkey_i *src) { return bch2_bkey_buf_copy_noprof(s, src); }
int bch2_bkey_buf_unpack_noprof__extern(struct bkey_buf *s, struct btree *b, struct bkey_packed *src) { return bch2_bkey_buf_unpack_noprof(s, b, src); }
void bch2_bkey_buf_init__extern(struct bkey_buf *s) { bch2_bkey_buf_init(s); }
void bch2_bkey_buf_exit__extern(struct bkey_buf *s) { bch2_bkey_buf_exit(s); }
int bch2_read_indirect_extent__extern(struct btree_trans *trans, enum btree_id *data_btree, s64 *offset_into_extent, struct bkey_buf *extent) { return bch2_read_indirect_extent(trans, data_btree, offset_into_extent, extent); }
int bch2_read_extent__extern(struct btree_trans *trans, struct bch_read_bio *rbio, struct bpos read_pos, enum btree_id data_btree, struct bkey_s_c k, unsigned int offset_into_extent, enum bch_read_flags flags) { return bch2_read_extent(trans, rbio, read_pos, data_btree, k, offset_into_extent, flags); }
void bch2_write_op_init__extern(struct bch_write_op *op, struct bch_fs *c, struct bch_inode_opts opts) { bch2_write_op_init(op, c, opts); }
void bch2_account_io_success_fail__extern(struct bch_dev *ca, enum bch_member_error_type type, bool success) { bch2_account_io_success_fail(ca, type, success); }
void bch2_account_io_completion__extern(struct bch_dev *ca, enum bch_member_error_type type, u64 submit_time, bool success) { bch2_account_io_completion(ca, type, submit_time, success); }
int bch2_recovery_cancelled__extern(struct bch_fs *c) { return bch2_recovery_cancelled(c); }
int bch2_inode_has_child_snapshots__extern(struct btree_trans *trans, struct bpos pos) { return bch2_inode_has_child_snapshots(trans, pos); }
int bch2_inode_peek_nowarn__extern(struct btree_trans *trans, struct btree_iter *iter, struct bch_inode_unpacked *inode, subvol_inum inum, unsigned int flags) { return bch2_inode_peek_nowarn(trans, iter, inode, inum, flags); }
int bch2_inode_find_by_inum_nowarn_trans__extern(struct btree_trans *trans, subvol_inum inum, struct bch_inode_unpacked *inode) { return bch2_inode_find_by_inum_nowarn_trans(trans, inum, inode); }
int bch2_inode_write__extern(struct btree_trans *trans, struct btree_iter *iter, struct bch_inode_unpacked *inode) { return bch2_inode_write(trans, iter, inode); }
void bch2_inode_opt_set__extern(struct bch_inode_unpacked *inode, enum inode_opt_id id, u64 v) { bch2_inode_opt_set(inode, id, v); }
u64 bch2_inode_opt_get__extern(struct bch_inode_unpacked *inode, enum inode_opt_id id) { return bch2_inode_opt_get(inode, id); }
u32 bch2_inode_flags__extern(struct bkey_s_c k) { return bch2_inode_flags(k); }
bool bch2_inode_casefold__extern(struct bch_fs *c, const struct bch_inode_unpacked *bi) { return bch2_inode_casefold(c, bi); }
bool bch2_inode_has_backpointer__extern(const struct bch_inode_unpacked *bi) { return bch2_inode_has_backpointer(bi); }
unsigned int bch2_inode_nlink_get__extern(struct bch_inode_unpacked *bi) { return bch2_inode_nlink_get(bi); }
void bch2_inode_nlink_set__extern(struct bch_inode_unpacked *bi, unsigned int nlink) { bch2_inode_nlink_set(bi, nlink); }
int bch2_trigger_extent_reconcile__extern(struct btree_trans *trans, struct btree_trigger_op op) { return bch2_trigger_extent_reconcile(trans, op); }
struct bch_extent_reconcile bch2_inode_reconcile_opts_get__extern(struct bch_fs *c, struct bch_inode_unpacked *inode) { return bch2_inode_reconcile_opts_get(c, inode); }
struct bkey_s_c bch2_btree_iter_peek_in_subvolume_max_type__extern(struct btree_iter *iter, struct bpos end, u32 subvolid, u32 *snapshot, unsigned int flags) { return bch2_btree_iter_peek_in_subvolume_max_type(iter, end, subvolid, snapshot, flags); }
enum bch_str_hash_type bch2_str_hash_opt_to_type__extern(struct bch_fs *c, enum bch_str_hash_opts opt) { return bch2_str_hash_opt_to_type(c, opt); }
void bch2_str_hash_init__extern(struct bch_str_hash_ctx *ctx, const struct bch_hash_info *info) { bch2_str_hash_init(ctx, info); }
void bch2_str_hash_update__extern(struct bch_str_hash_ctx *ctx, const struct bch_hash_info *info, const void *data, size_t len) { bch2_str_hash_update(ctx, info, data, len); }
u64 __bch2_str_hash_end__extern(struct bch_str_hash_ctx *ctx, const struct bch_hash_info *info) { return __bch2_str_hash_end(ctx, info); }
u64 bch2_str_hash_end__extern(struct bch_str_hash_ctx *ctx, const struct bch_hash_info *info, bool maybe_31bit) { return bch2_str_hash_end(ctx, info, maybe_31bit); }
struct bkey_s_c bch2_hash_lookup_in_snapshot__extern(struct btree_trans *trans, struct btree_iter *iter, const struct bch_hash_desc desc, const struct bch_hash_info *info, subvol_inum inum, const void *key, enum btree_iter_update_trigger_flags flags, u32 snapshot) { return bch2_hash_lookup_in_snapshot(trans, iter, desc, info, inum, key, flags, snapshot); }
struct bkey_s_c bch2_hash_lookup__extern(struct btree_trans *trans, struct btree_iter *iter, const struct bch_hash_desc desc, const struct bch_hash_info *info, subvol_inum inum, const void *key, enum btree_iter_update_trigger_flags flags) { return bch2_hash_lookup(trans, iter, desc, info, inum, key, flags); }
int bch2_hash_hole__extern(struct btree_trans *trans, struct btree_iter *iter, const struct bch_hash_desc desc, const struct bch_hash_info *info, subvol_inum inum, const void *key) { return bch2_hash_hole(trans, iter, desc, info, inum, key); }
int bch2_hash_needs_whiteout__extern(struct btree_trans *trans, const struct bch_hash_desc desc, const struct bch_hash_info *info, struct btree_iter *start) { return bch2_hash_needs_whiteout(trans, desc, info, start); }
struct bkey_s_c bch2_hash_set_or_get_in_snapshot__extern(struct btree_trans *trans, struct btree_iter *iter, const struct bch_hash_desc desc, const struct bch_hash_info *info, subvol_inum inum, u32 snapshot, struct bkey_i *insert, enum btree_iter_update_trigger_flags flags) { return bch2_hash_set_or_get_in_snapshot(trans, iter, desc, info, inum, snapshot, insert, flags); }
int bch2_hash_set_in_snapshot__extern(struct btree_trans *trans, const struct bch_hash_desc desc, const struct bch_hash_info *info, subvol_inum inum, u32 snapshot, struct bkey_i *insert, enum btree_iter_update_trigger_flags flags) { return bch2_hash_set_in_snapshot(trans, desc, info, inum, snapshot, insert, flags); }
int bch2_hash_set__extern(struct btree_trans *trans, const struct bch_hash_desc desc, const struct bch_hash_info *info, subvol_inum inum, struct bkey_i *insert, enum btree_iter_update_trigger_flags flags) { return bch2_hash_set(trans, desc, info, inum, insert, flags); }
int bch2_hash_delete_at__extern(struct btree_trans *trans, const struct bch_hash_desc desc, const struct bch_hash_info *info, struct btree_iter *iter, enum btree_iter_update_trigger_flags flags) { return bch2_hash_delete_at(trans, desc, info, iter, flags); }
int bch2_hash_delete__extern(struct btree_trans *trans, const struct bch_hash_desc desc, const struct bch_hash_info *info, subvol_inum inum, const void *key) { return bch2_hash_delete(trans, desc, info, inum, key); }
int bch2_str_hash_check_key__extern(struct btree_trans *trans, struct snapshots_seen *s, const struct bch_hash_desc *desc, struct bch_hash_info *hash_info, struct bkey_s_c hash_k, bool *updated_before_k_pos, bool *repaired_inode) { return bch2_str_hash_check_key(trans, s, desc, hash_info, hash_k, updated_before_k_pos, repaired_inode); }
int bch2_maybe_casefold__extern(struct btree_trans *trans, const struct bch_hash_info *info, const struct qstr *str, struct qstr *out_cf) { return bch2_maybe_casefold(trans, info, str, out_cf); }
int bch2_check_dirent_target__extern(struct btree_trans *trans, struct btree_iter *dirent_iter, struct bkey_s_c_dirent d, struct bch_inode_unpacked *target, bool in_fsck) { return bch2_check_dirent_target(trans, dirent_iter, d, target, in_fsck); }
void bch2_replicas_entry_cached__extern(struct bch_replicas_entry_v1 *e, unsigned int dev) { bch2_replicas_entry_cached(e, dev); }
void bch2_replicas_entry_put__extern(struct bch_fs *c, struct bch_replicas_entry_v1 *r) { bch2_replicas_entry_put(c, r); }
bool bch2_replicas_entry_has_dev__extern(struct bch_replicas_entry_v1 *r, unsigned int dev) { return bch2_replicas_entry_has_dev(r, dev); }
bool bch2_replicas_entry_eq__extern(struct bch_replicas_entry_v1 *l, struct bch_replicas_entry_v1 *r) { return bch2_replicas_entry_eq(l, r); }
bool __bch2_journal_pin_put__extern(struct journal *j, u64 seq) { return __bch2_journal_pin_put(j, seq); }
void bch2_journal_pin_add__extern(struct journal *j, u64 seq, struct journal_entry_pin *pin, journal_pin_flush_fn flush_fn) { bch2_journal_pin_add(j, seq, pin, flush_fn); }
void bch2_journal_pin_update__extern(struct journal *j, u64 seq, struct journal_entry_pin *pin, journal_pin_flush_fn flush_fn) { bch2_journal_pin_update(j, seq, pin, flush_fn); }
bool bch2_journal_flush_all_pins__extern(struct journal *j) { return bch2_journal_flush_all_pins(j); }
bool bch2_journal_flush_outstanding_pins__extern(struct journal *j) { return bch2_journal_flush_outstanding_pins(j); }
