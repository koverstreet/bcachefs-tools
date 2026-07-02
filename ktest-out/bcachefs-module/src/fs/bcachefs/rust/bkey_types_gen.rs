// Auto-generated from BCH_BKEY_TYPES() — do not edit

pub type BkeyDeleted = Bkey<c::bkey_i_deleted>;
pub type BkeyWhiteout = Bkey<c::bkey_i_whiteout>;
pub type BkeyError = Bkey<c::bkey_i_error>;
pub type BkeyCookie = Bkey<c::bkey_i_cookie>;
pub type BkeyHashWhiteout = Bkey<c::bkey_i_hash_whiteout>;
pub type BkeyBtreePtr = Bkey<c::bkey_i_btree_ptr>;
pub type BkeyExtent = Bkey<c::bkey_i_extent>;
pub type BkeyReservation = Bkey<c::bkey_i_reservation>;
pub type BkeyInode = Bkey<c::bkey_i_inode>;
pub type BkeyInodeGeneration = Bkey<c::bkey_i_inode_generation>;
pub type BkeyDirent = Bkey<c::bkey_i_dirent>;
pub type BkeyXattr = Bkey<c::bkey_i_xattr>;
pub type BkeyAlloc = Bkey<c::bkey_i_alloc>;
pub type BkeyQuota = Bkey<c::bkey_i_quota>;
pub type BkeyStripe = Bkey<c::bkey_i_stripe>;
pub type BkeyReflinkP = Bkey<c::bkey_i_reflink_p>;
pub type BkeyReflinkV = Bkey<c::bkey_i_reflink_v>;
pub type BkeyInlineData = Bkey<c::bkey_i_inline_data>;
pub type BkeyBtreePtrV2 = Bkey<c::bkey_i_btree_ptr_v2>;
pub type BkeyIndirectInlineData = Bkey<c::bkey_i_indirect_inline_data>;
pub type BkeyAllocV2 = Bkey<c::bkey_i_alloc_v2>;
pub type BkeySubvolume = Bkey<c::bkey_i_subvolume>;
pub type BkeySnapshot = Bkey<c::bkey_i_snapshot>;
pub type BkeyInodeV2 = Bkey<c::bkey_i_inode_v2>;
pub type BkeyAllocV3 = Bkey<c::bkey_i_alloc_v3>;
pub type BkeySet = Bkey<c::bkey_i_set>;
pub type BkeyLru = Bkey<c::bkey_i_lru>;
pub type BkeyAllocV4 = Bkey<c::bkey_i_alloc_v4>;
pub type BkeyBackpointer = Bkey<c::bkey_i_backpointer>;
pub type BkeyInodeV3 = Bkey<c::bkey_i_inode_v3>;
pub type BkeyBucketGens = Bkey<c::bkey_i_bucket_gens>;
pub type BkeySnapshotTree = Bkey<c::bkey_i_snapshot_tree>;
pub type BkeyLoggedOpTruncate = Bkey<c::bkey_i_logged_op_truncate>;
pub type BkeyLoggedOpFinsert = Bkey<c::bkey_i_logged_op_finsert>;
pub type BkeyAccounting = Bkey<c::bkey_i_accounting>;
pub type BkeyInodeAllocCursor = Bkey<c::bkey_i_inode_alloc_cursor>;
pub type BkeyExtentWhiteout = Bkey<c::bkey_i_extent_whiteout>;
pub type BkeyLoggedOpStripeUpdate = Bkey<c::bkey_i_logged_op_stripe_update>;

impl c::bkey_i_deleted {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_whiteout {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_error {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_cookie {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_hash_whiteout {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_btree_ptr {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_extent {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_reservation {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_inode {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_inode_generation {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_dirent {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_xattr {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_alloc {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_quota {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_stripe {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_reflink_p {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_reflink_v {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_inline_data {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_btree_ptr_v2 {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_indirect_inline_data {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_alloc_v2 {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_subvolume {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_snapshot {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_inode_v2 {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_alloc_v3 {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_set {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_lru {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_alloc_v4 {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_backpointer {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_inode_v3 {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_bucket_gens {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_snapshot_tree {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_logged_op_truncate {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_logged_op_finsert {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_accounting {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_inode_alloc_cursor {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_extent_whiteout {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

impl c::bkey_i_logged_op_stripe_update {
    pub fn k(&self) -> &c::bkey { unsafe { self.__bindgen_anon_1.k.as_ref() } }
    pub fn k_mut(&mut self) -> &mut c::bkey { unsafe { self.__bindgen_anon_1.k.as_mut() } }
    pub fn k_i(&self) -> &c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_ref() } }
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { self.__bindgen_anon_1.k_i.as_mut() } }
}

pub trait BkeyInit: Default {
    fn init(&mut self);
    fn k(&self) -> &c::bkey;
    fn k_mut(&mut self) -> &mut c::bkey;
    fn k_i(&self) -> &c::bkey_i;
    fn k_i_mut(&mut self) -> &mut c::bkey_i;
}

impl BkeyInit for c::bkey_i_deleted {
    fn init(&mut self) { unsafe { c::bkey_deleted_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_deleted::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_deleted::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_deleted::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_deleted::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_whiteout {
    fn init(&mut self) { unsafe { c::bkey_whiteout_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_whiteout::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_whiteout::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_whiteout::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_whiteout::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_error {
    fn init(&mut self) { unsafe { c::bkey_error_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_error::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_error::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_error::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_error::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_cookie {
    fn init(&mut self) { unsafe { c::bkey_cookie_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_cookie::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_cookie::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_cookie::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_cookie::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_hash_whiteout {
    fn init(&mut self) { unsafe { c::bkey_hash_whiteout_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_hash_whiteout::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_hash_whiteout::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_hash_whiteout::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_hash_whiteout::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_btree_ptr {
    fn init(&mut self) { unsafe { c::bkey_btree_ptr_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_btree_ptr::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_btree_ptr::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_btree_ptr::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_btree_ptr::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_extent {
    fn init(&mut self) { unsafe { c::bkey_extent_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_extent::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_extent::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_extent::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_extent::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_reservation {
    fn init(&mut self) { unsafe { c::bkey_reservation_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_reservation::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_reservation::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_reservation::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_reservation::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_inode {
    fn init(&mut self) { unsafe { c::bkey_inode_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_inode::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_inode::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_inode::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_inode::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_inode_generation {
    fn init(&mut self) { unsafe { c::bkey_inode_generation_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_inode_generation::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_inode_generation::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_inode_generation::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_inode_generation::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_dirent {
    fn init(&mut self) { unsafe { c::bkey_dirent_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_dirent::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_dirent::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_dirent::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_dirent::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_xattr {
    fn init(&mut self) { unsafe { c::bkey_xattr_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_xattr::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_xattr::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_xattr::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_xattr::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_alloc {
    fn init(&mut self) { unsafe { c::bkey_alloc_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_alloc::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_alloc::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_alloc::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_alloc::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_quota {
    fn init(&mut self) { unsafe { c::bkey_quota_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_quota::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_quota::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_quota::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_quota::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_stripe {
    fn init(&mut self) { unsafe { c::bkey_stripe_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_stripe::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_stripe::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_stripe::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_stripe::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_reflink_p {
    fn init(&mut self) { unsafe { c::bkey_reflink_p_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_reflink_p::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_reflink_p::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_reflink_p::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_reflink_p::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_reflink_v {
    fn init(&mut self) { unsafe { c::bkey_reflink_v_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_reflink_v::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_reflink_v::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_reflink_v::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_reflink_v::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_inline_data {
    fn init(&mut self) { unsafe { c::bkey_inline_data_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_inline_data::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_inline_data::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_inline_data::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_inline_data::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_btree_ptr_v2 {
    fn init(&mut self) { unsafe { c::bkey_btree_ptr_v2_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_btree_ptr_v2::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_btree_ptr_v2::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_btree_ptr_v2::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_btree_ptr_v2::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_indirect_inline_data {
    fn init(&mut self) { unsafe { c::bkey_indirect_inline_data_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_indirect_inline_data::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_indirect_inline_data::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_indirect_inline_data::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_indirect_inline_data::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_alloc_v2 {
    fn init(&mut self) { unsafe { c::bkey_alloc_v2_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_alloc_v2::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_alloc_v2::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_alloc_v2::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_alloc_v2::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_subvolume {
    fn init(&mut self) { unsafe { c::bkey_subvolume_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_subvolume::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_subvolume::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_subvolume::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_subvolume::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_snapshot {
    fn init(&mut self) { unsafe { c::bkey_snapshot_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_snapshot::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_snapshot::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_snapshot::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_snapshot::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_inode_v2 {
    fn init(&mut self) { unsafe { c::bkey_inode_v2_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_inode_v2::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_inode_v2::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_inode_v2::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_inode_v2::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_alloc_v3 {
    fn init(&mut self) { unsafe { c::bkey_alloc_v3_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_alloc_v3::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_alloc_v3::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_alloc_v3::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_alloc_v3::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_set {
    fn init(&mut self) { unsafe { c::bkey_set_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_set::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_set::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_set::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_set::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_lru {
    fn init(&mut self) { unsafe { c::bkey_lru_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_lru::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_lru::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_lru::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_lru::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_alloc_v4 {
    fn init(&mut self) { unsafe { c::bkey_alloc_v4_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_alloc_v4::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_alloc_v4::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_alloc_v4::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_alloc_v4::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_backpointer {
    fn init(&mut self) { unsafe { c::bkey_backpointer_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_backpointer::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_backpointer::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_backpointer::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_backpointer::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_inode_v3 {
    fn init(&mut self) { unsafe { c::bkey_inode_v3_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_inode_v3::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_inode_v3::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_inode_v3::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_inode_v3::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_bucket_gens {
    fn init(&mut self) { unsafe { c::bkey_bucket_gens_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_bucket_gens::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_bucket_gens::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_bucket_gens::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_bucket_gens::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_snapshot_tree {
    fn init(&mut self) { unsafe { c::bkey_snapshot_tree_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_snapshot_tree::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_snapshot_tree::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_snapshot_tree::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_snapshot_tree::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_logged_op_truncate {
    fn init(&mut self) { unsafe { c::bkey_logged_op_truncate_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_logged_op_truncate::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_logged_op_truncate::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_logged_op_truncate::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_logged_op_truncate::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_logged_op_finsert {
    fn init(&mut self) { unsafe { c::bkey_logged_op_finsert_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_logged_op_finsert::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_logged_op_finsert::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_logged_op_finsert::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_logged_op_finsert::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_accounting {
    fn init(&mut self) { unsafe { c::bkey_accounting_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_accounting::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_accounting::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_accounting::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_accounting::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_inode_alloc_cursor {
    fn init(&mut self) { unsafe { c::bkey_inode_alloc_cursor_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_inode_alloc_cursor::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_inode_alloc_cursor::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_inode_alloc_cursor::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_inode_alloc_cursor::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_extent_whiteout {
    fn init(&mut self) { unsafe { c::bkey_extent_whiteout_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_extent_whiteout::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_extent_whiteout::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_extent_whiteout::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_extent_whiteout::k_i_mut(self) }
}

impl BkeyInit for c::bkey_i_logged_op_stripe_update {
    fn init(&mut self) { unsafe { c::bkey_logged_op_stripe_update_init(self.k_i_mut()) }; }
    fn k(&self) -> &c::bkey { c::bkey_i_logged_op_stripe_update::k(self) }
    fn k_mut(&mut self) -> &mut c::bkey { c::bkey_i_logged_op_stripe_update::k_mut(self) }
    fn k_i(&self) -> &c::bkey_i { c::bkey_i_logged_op_stripe_update::k_i(self) }
    fn k_i_mut(&mut self) -> &mut c::bkey_i { c::bkey_i_logged_op_stripe_update::k_i_mut(self) }
}

/// Typed dispatch for inline bkeys (`bkey_i`).
pub enum BkeyValI<'a> {
    deleted(&'a c::bkey_i_deleted),
    whiteout(&'a c::bkey_i_whiteout),
    error(&'a c::bkey_i_error),
    cookie(&'a c::bkey_i_cookie),
    hash_whiteout(&'a c::bkey_i_hash_whiteout),
    btree_ptr(&'a c::bkey_i_btree_ptr),
    extent(&'a c::bkey_i_extent),
    reservation(&'a c::bkey_i_reservation),
    inode(&'a c::bkey_i_inode),
    inode_generation(&'a c::bkey_i_inode_generation),
    dirent(&'a c::bkey_i_dirent),
    xattr(&'a c::bkey_i_xattr),
    alloc(&'a c::bkey_i_alloc),
    quota(&'a c::bkey_i_quota),
    stripe(&'a c::bkey_i_stripe),
    reflink_p(&'a c::bkey_i_reflink_p),
    reflink_v(&'a c::bkey_i_reflink_v),
    inline_data(&'a c::bkey_i_inline_data),
    btree_ptr_v2(&'a c::bkey_i_btree_ptr_v2),
    indirect_inline_data(&'a c::bkey_i_indirect_inline_data),
    alloc_v2(&'a c::bkey_i_alloc_v2),
    subvolume(&'a c::bkey_i_subvolume),
    snapshot(&'a c::bkey_i_snapshot),
    inode_v2(&'a c::bkey_i_inode_v2),
    alloc_v3(&'a c::bkey_i_alloc_v3),
    set(&'a c::bkey_i_set),
    lru(&'a c::bkey_i_lru),
    alloc_v4(&'a c::bkey_i_alloc_v4),
    backpointer(&'a c::bkey_i_backpointer),
    inode_v3(&'a c::bkey_i_inode_v3),
    bucket_gens(&'a c::bkey_i_bucket_gens),
    snapshot_tree(&'a c::bkey_i_snapshot_tree),
    logged_op_truncate(&'a c::bkey_i_logged_op_truncate),
    logged_op_finsert(&'a c::bkey_i_logged_op_finsert),
    accounting(&'a c::bkey_i_accounting),
    inode_alloc_cursor(&'a c::bkey_i_inode_alloc_cursor),
    extent_whiteout(&'a c::bkey_i_extent_whiteout),
    logged_op_stripe_update(&'a c::bkey_i_logged_op_stripe_update),
    unknown(&'a c::bkey_i),
}

impl<'a> BkeyValI<'a> {
    #[allow(clippy::missing_transmute_annotations)]
    pub fn from_bkey_i(k: &'a c::bkey_i) -> Self {
        match k.k.type_ as u32 {
            0 => BkeyValI::deleted(unsafe { core::mem::transmute(k) }),
            1 => BkeyValI::whiteout(unsafe { core::mem::transmute(k) }),
            2 => BkeyValI::error(unsafe { core::mem::transmute(k) }),
            3 => BkeyValI::cookie(unsafe { core::mem::transmute(k) }),
            4 => BkeyValI::hash_whiteout(unsafe { core::mem::transmute(k) }),
            5 => BkeyValI::btree_ptr(unsafe { core::mem::transmute(k) }),
            6 => BkeyValI::extent(unsafe { core::mem::transmute(k) }),
            7 => BkeyValI::reservation(unsafe { core::mem::transmute(k) }),
            8 => BkeyValI::inode(unsafe { core::mem::transmute(k) }),
            9 => BkeyValI::inode_generation(unsafe { core::mem::transmute(k) }),
            10 => BkeyValI::dirent(unsafe { core::mem::transmute(k) }),
            11 => BkeyValI::xattr(unsafe { core::mem::transmute(k) }),
            12 => BkeyValI::alloc(unsafe { core::mem::transmute(k) }),
            13 => BkeyValI::quota(unsafe { core::mem::transmute(k) }),
            14 => BkeyValI::stripe(unsafe { core::mem::transmute(k) }),
            15 => BkeyValI::reflink_p(unsafe { core::mem::transmute(k) }),
            16 => BkeyValI::reflink_v(unsafe { core::mem::transmute(k) }),
            17 => BkeyValI::inline_data(unsafe { core::mem::transmute(k) }),
            18 => BkeyValI::btree_ptr_v2(unsafe { core::mem::transmute(k) }),
            19 => BkeyValI::indirect_inline_data(unsafe { core::mem::transmute(k) }),
            20 => BkeyValI::alloc_v2(unsafe { core::mem::transmute(k) }),
            21 => BkeyValI::subvolume(unsafe { core::mem::transmute(k) }),
            22 => BkeyValI::snapshot(unsafe { core::mem::transmute(k) }),
            23 => BkeyValI::inode_v2(unsafe { core::mem::transmute(k) }),
            24 => BkeyValI::alloc_v3(unsafe { core::mem::transmute(k) }),
            25 => BkeyValI::set(unsafe { core::mem::transmute(k) }),
            26 => BkeyValI::lru(unsafe { core::mem::transmute(k) }),
            27 => BkeyValI::alloc_v4(unsafe { core::mem::transmute(k) }),
            28 => BkeyValI::backpointer(unsafe { core::mem::transmute(k) }),
            29 => BkeyValI::inode_v3(unsafe { core::mem::transmute(k) }),
            30 => BkeyValI::bucket_gens(unsafe { core::mem::transmute(k) }),
            31 => BkeyValI::snapshot_tree(unsafe { core::mem::transmute(k) }),
            32 => BkeyValI::logged_op_truncate(unsafe { core::mem::transmute(k) }),
            33 => BkeyValI::logged_op_finsert(unsafe { core::mem::transmute(k) }),
            34 => BkeyValI::accounting(unsafe { core::mem::transmute(k) }),
            35 => BkeyValI::inode_alloc_cursor(unsafe { core::mem::transmute(k) }),
            36 => BkeyValI::extent_whiteout(unsafe { core::mem::transmute(k) }),
            37 => BkeyValI::logged_op_stripe_update(unsafe { core::mem::transmute(k) }),
            _ => BkeyValI::unknown(k),
        }
    }
}

/// Typed dispatch for mutable inline bkeys (`bkey_i`).
pub enum BkeyValIMut<'a> {
    deleted(&'a mut c::bkey_i_deleted),
    whiteout(&'a mut c::bkey_i_whiteout),
    error(&'a mut c::bkey_i_error),
    cookie(&'a mut c::bkey_i_cookie),
    hash_whiteout(&'a mut c::bkey_i_hash_whiteout),
    btree_ptr(&'a mut c::bkey_i_btree_ptr),
    extent(&'a mut c::bkey_i_extent),
    reservation(&'a mut c::bkey_i_reservation),
    inode(&'a mut c::bkey_i_inode),
    inode_generation(&'a mut c::bkey_i_inode_generation),
    dirent(&'a mut c::bkey_i_dirent),
    xattr(&'a mut c::bkey_i_xattr),
    alloc(&'a mut c::bkey_i_alloc),
    quota(&'a mut c::bkey_i_quota),
    stripe(&'a mut c::bkey_i_stripe),
    reflink_p(&'a mut c::bkey_i_reflink_p),
    reflink_v(&'a mut c::bkey_i_reflink_v),
    inline_data(&'a mut c::bkey_i_inline_data),
    btree_ptr_v2(&'a mut c::bkey_i_btree_ptr_v2),
    indirect_inline_data(&'a mut c::bkey_i_indirect_inline_data),
    alloc_v2(&'a mut c::bkey_i_alloc_v2),
    subvolume(&'a mut c::bkey_i_subvolume),
    snapshot(&'a mut c::bkey_i_snapshot),
    inode_v2(&'a mut c::bkey_i_inode_v2),
    alloc_v3(&'a mut c::bkey_i_alloc_v3),
    set(&'a mut c::bkey_i_set),
    lru(&'a mut c::bkey_i_lru),
    alloc_v4(&'a mut c::bkey_i_alloc_v4),
    backpointer(&'a mut c::bkey_i_backpointer),
    inode_v3(&'a mut c::bkey_i_inode_v3),
    bucket_gens(&'a mut c::bkey_i_bucket_gens),
    snapshot_tree(&'a mut c::bkey_i_snapshot_tree),
    logged_op_truncate(&'a mut c::bkey_i_logged_op_truncate),
    logged_op_finsert(&'a mut c::bkey_i_logged_op_finsert),
    accounting(&'a mut c::bkey_i_accounting),
    inode_alloc_cursor(&'a mut c::bkey_i_inode_alloc_cursor),
    extent_whiteout(&'a mut c::bkey_i_extent_whiteout),
    logged_op_stripe_update(&'a mut c::bkey_i_logged_op_stripe_update),
    unknown(&'a mut c::bkey_i),
}

impl<'a> BkeyValIMut<'a> {
    #[allow(clippy::missing_transmute_annotations)]
    pub fn from_bkey_i(k: &'a mut c::bkey_i) -> Self {
        let type_ = k.k.type_;
        match type_ as u32 {
            0 => BkeyValIMut::deleted(unsafe { core::mem::transmute(k) }),
            1 => BkeyValIMut::whiteout(unsafe { core::mem::transmute(k) }),
            2 => BkeyValIMut::error(unsafe { core::mem::transmute(k) }),
            3 => BkeyValIMut::cookie(unsafe { core::mem::transmute(k) }),
            4 => BkeyValIMut::hash_whiteout(unsafe { core::mem::transmute(k) }),
            5 => BkeyValIMut::btree_ptr(unsafe { core::mem::transmute(k) }),
            6 => BkeyValIMut::extent(unsafe { core::mem::transmute(k) }),
            7 => BkeyValIMut::reservation(unsafe { core::mem::transmute(k) }),
            8 => BkeyValIMut::inode(unsafe { core::mem::transmute(k) }),
            9 => BkeyValIMut::inode_generation(unsafe { core::mem::transmute(k) }),
            10 => BkeyValIMut::dirent(unsafe { core::mem::transmute(k) }),
            11 => BkeyValIMut::xattr(unsafe { core::mem::transmute(k) }),
            12 => BkeyValIMut::alloc(unsafe { core::mem::transmute(k) }),
            13 => BkeyValIMut::quota(unsafe { core::mem::transmute(k) }),
            14 => BkeyValIMut::stripe(unsafe { core::mem::transmute(k) }),
            15 => BkeyValIMut::reflink_p(unsafe { core::mem::transmute(k) }),
            16 => BkeyValIMut::reflink_v(unsafe { core::mem::transmute(k) }),
            17 => BkeyValIMut::inline_data(unsafe { core::mem::transmute(k) }),
            18 => BkeyValIMut::btree_ptr_v2(unsafe { core::mem::transmute(k) }),
            19 => BkeyValIMut::indirect_inline_data(unsafe { core::mem::transmute(k) }),
            20 => BkeyValIMut::alloc_v2(unsafe { core::mem::transmute(k) }),
            21 => BkeyValIMut::subvolume(unsafe { core::mem::transmute(k) }),
            22 => BkeyValIMut::snapshot(unsafe { core::mem::transmute(k) }),
            23 => BkeyValIMut::inode_v2(unsafe { core::mem::transmute(k) }),
            24 => BkeyValIMut::alloc_v3(unsafe { core::mem::transmute(k) }),
            25 => BkeyValIMut::set(unsafe { core::mem::transmute(k) }),
            26 => BkeyValIMut::lru(unsafe { core::mem::transmute(k) }),
            27 => BkeyValIMut::alloc_v4(unsafe { core::mem::transmute(k) }),
            28 => BkeyValIMut::backpointer(unsafe { core::mem::transmute(k) }),
            29 => BkeyValIMut::inode_v3(unsafe { core::mem::transmute(k) }),
            30 => BkeyValIMut::bucket_gens(unsafe { core::mem::transmute(k) }),
            31 => BkeyValIMut::snapshot_tree(unsafe { core::mem::transmute(k) }),
            32 => BkeyValIMut::logged_op_truncate(unsafe { core::mem::transmute(k) }),
            33 => BkeyValIMut::logged_op_finsert(unsafe { core::mem::transmute(k) }),
            34 => BkeyValIMut::accounting(unsafe { core::mem::transmute(k) }),
            35 => BkeyValIMut::inode_alloc_cursor(unsafe { core::mem::transmute(k) }),
            36 => BkeyValIMut::extent_whiteout(unsafe { core::mem::transmute(k) }),
            37 => BkeyValIMut::logged_op_stripe_update(unsafe { core::mem::transmute(k) }),
            _ => BkeyValIMut::unknown(k),
        }
    }
}

/// Typed dispatch for split-const bkey references.
pub enum BkeyValSC<'a> {
    deleted(&'a c::bkey, &'a c::bch_deleted),
    whiteout(&'a c::bkey, &'a c::bch_whiteout),
    error(&'a c::bkey, &'a c::bch_error),
    cookie(&'a c::bkey, &'a c::bch_cookie),
    hash_whiteout(&'a c::bkey, &'a c::bch_hash_whiteout),
    btree_ptr(&'a c::bkey, &'a c::bch_btree_ptr),
    extent(&'a c::bkey, &'a c::bch_extent),
    reservation(&'a c::bkey, &'a c::bch_reservation),
    inode(&'a c::bkey, &'a c::bch_inode),
    inode_generation(&'a c::bkey, &'a c::bch_inode_generation),
    dirent(&'a c::bkey, &'a c::bch_dirent),
    xattr(&'a c::bkey, &'a c::bch_xattr),
    alloc(&'a c::bkey, &'a c::bch_alloc),
    quota(&'a c::bkey, &'a c::bch_quota),
    stripe(&'a c::bkey, &'a c::bch_stripe),
    reflink_p(&'a c::bkey, &'a c::bch_reflink_p),
    reflink_v(&'a c::bkey, &'a c::bch_reflink_v),
    inline_data(&'a c::bkey, &'a c::bch_inline_data),
    btree_ptr_v2(&'a c::bkey, &'a c::bch_btree_ptr_v2),
    indirect_inline_data(&'a c::bkey, &'a c::bch_indirect_inline_data),
    alloc_v2(&'a c::bkey, &'a c::bch_alloc_v2),
    subvolume(&'a c::bkey, &'a c::bch_subvolume),
    snapshot(&'a c::bkey, &'a c::bch_snapshot),
    inode_v2(&'a c::bkey, &'a c::bch_inode_v2),
    alloc_v3(&'a c::bkey, &'a c::bch_alloc_v3),
    set(&'a c::bkey, &'a c::bch_set),
    lru(&'a c::bkey, &'a c::bch_lru),
    alloc_v4(&'a c::bkey, &'a c::bch_alloc_v4),
    backpointer(&'a c::bkey, &'a c::bch_backpointer),
    inode_v3(&'a c::bkey, &'a c::bch_inode_v3),
    bucket_gens(&'a c::bkey, &'a c::bch_bucket_gens),
    snapshot_tree(&'a c::bkey, &'a c::bch_snapshot_tree),
    logged_op_truncate(&'a c::bkey, &'a c::bch_logged_op_truncate),
    logged_op_finsert(&'a c::bkey, &'a c::bch_logged_op_finsert),
    accounting(&'a c::bkey, &'a c::bch_accounting),
    inode_alloc_cursor(&'a c::bkey, &'a c::bch_inode_alloc_cursor),
    extent_whiteout(&'a c::bkey, &'a c::bch_extent_whiteout),
    logged_op_stripe_update(&'a c::bkey, &'a c::bch_logged_op_stripe_update),
    unknown(&'a c::bkey, u8),
}

impl<'a> BkeyValSC<'a> {
    #[allow(clippy::missing_transmute_annotations)]
    pub fn from_bkey_i(k: &'a c::bkey_i) -> Self {
        match k.k.type_ as u32 {
            0 => BkeyValSC::deleted(&k.k, unsafe { core::mem::transmute(&k.v) }),
            1 => BkeyValSC::whiteout(&k.k, unsafe { core::mem::transmute(&k.v) }),
            2 => BkeyValSC::error(&k.k, unsafe { core::mem::transmute(&k.v) }),
            3 => BkeyValSC::cookie(&k.k, unsafe { core::mem::transmute(&k.v) }),
            4 => BkeyValSC::hash_whiteout(&k.k, unsafe { core::mem::transmute(&k.v) }),
            5 => BkeyValSC::btree_ptr(&k.k, unsafe { core::mem::transmute(&k.v) }),
            6 => BkeyValSC::extent(&k.k, unsafe { core::mem::transmute(&k.v) }),
            7 => BkeyValSC::reservation(&k.k, unsafe { core::mem::transmute(&k.v) }),
            8 => BkeyValSC::inode(&k.k, unsafe { core::mem::transmute(&k.v) }),
            9 => BkeyValSC::inode_generation(&k.k, unsafe { core::mem::transmute(&k.v) }),
            10 => BkeyValSC::dirent(&k.k, unsafe { core::mem::transmute(&k.v) }),
            11 => BkeyValSC::xattr(&k.k, unsafe { core::mem::transmute(&k.v) }),
            12 => BkeyValSC::alloc(&k.k, unsafe { core::mem::transmute(&k.v) }),
            13 => BkeyValSC::quota(&k.k, unsafe { core::mem::transmute(&k.v) }),
            14 => BkeyValSC::stripe(&k.k, unsafe { core::mem::transmute(&k.v) }),
            15 => BkeyValSC::reflink_p(&k.k, unsafe { core::mem::transmute(&k.v) }),
            16 => BkeyValSC::reflink_v(&k.k, unsafe { core::mem::transmute(&k.v) }),
            17 => BkeyValSC::inline_data(&k.k, unsafe { core::mem::transmute(&k.v) }),
            18 => BkeyValSC::btree_ptr_v2(&k.k, unsafe { core::mem::transmute(&k.v) }),
            19 => BkeyValSC::indirect_inline_data(&k.k, unsafe { core::mem::transmute(&k.v) }),
            20 => BkeyValSC::alloc_v2(&k.k, unsafe { core::mem::transmute(&k.v) }),
            21 => BkeyValSC::subvolume(&k.k, unsafe { core::mem::transmute(&k.v) }),
            22 => BkeyValSC::snapshot(&k.k, unsafe { core::mem::transmute(&k.v) }),
            23 => BkeyValSC::inode_v2(&k.k, unsafe { core::mem::transmute(&k.v) }),
            24 => BkeyValSC::alloc_v3(&k.k, unsafe { core::mem::transmute(&k.v) }),
            25 => BkeyValSC::set(&k.k, unsafe { core::mem::transmute(&k.v) }),
            26 => BkeyValSC::lru(&k.k, unsafe { core::mem::transmute(&k.v) }),
            27 => BkeyValSC::alloc_v4(&k.k, unsafe { core::mem::transmute(&k.v) }),
            28 => BkeyValSC::backpointer(&k.k, unsafe { core::mem::transmute(&k.v) }),
            29 => BkeyValSC::inode_v3(&k.k, unsafe { core::mem::transmute(&k.v) }),
            30 => BkeyValSC::bucket_gens(&k.k, unsafe { core::mem::transmute(&k.v) }),
            31 => BkeyValSC::snapshot_tree(&k.k, unsafe { core::mem::transmute(&k.v) }),
            32 => BkeyValSC::logged_op_truncate(&k.k, unsafe { core::mem::transmute(&k.v) }),
            33 => BkeyValSC::logged_op_finsert(&k.k, unsafe { core::mem::transmute(&k.v) }),
            34 => BkeyValSC::accounting(&k.k, unsafe { core::mem::transmute(&k.v) }),
            35 => BkeyValSC::inode_alloc_cursor(&k.k, unsafe { core::mem::transmute(&k.v) }),
            36 => BkeyValSC::extent_whiteout(&k.k, unsafe { core::mem::transmute(&k.v) }),
            37 => BkeyValSC::logged_op_stripe_update(&k.k, unsafe { core::mem::transmute(&k.v) }),
            _ => BkeyValSC::unknown(&k.k, k.k.type_),
        }
    }

    /// Construct from raw key and value references.
    ///
    /// # Safety
    /// `val` must point to valid data for the bkey type indicated by `k.type_`.
    #[allow(clippy::missing_transmute_annotations)]
    pub unsafe fn from_raw(k: &'a c::bkey, val: &'a c::bch_val) -> Self {
        match k.type_ as u32 {
            0 => BkeyValSC::deleted(k, unsafe { core::mem::transmute(val) }),
            1 => BkeyValSC::whiteout(k, unsafe { core::mem::transmute(val) }),
            2 => BkeyValSC::error(k, unsafe { core::mem::transmute(val) }),
            3 => BkeyValSC::cookie(k, unsafe { core::mem::transmute(val) }),
            4 => BkeyValSC::hash_whiteout(k, unsafe { core::mem::transmute(val) }),
            5 => BkeyValSC::btree_ptr(k, unsafe { core::mem::transmute(val) }),
            6 => BkeyValSC::extent(k, unsafe { core::mem::transmute(val) }),
            7 => BkeyValSC::reservation(k, unsafe { core::mem::transmute(val) }),
            8 => BkeyValSC::inode(k, unsafe { core::mem::transmute(val) }),
            9 => BkeyValSC::inode_generation(k, unsafe { core::mem::transmute(val) }),
            10 => BkeyValSC::dirent(k, unsafe { core::mem::transmute(val) }),
            11 => BkeyValSC::xattr(k, unsafe { core::mem::transmute(val) }),
            12 => BkeyValSC::alloc(k, unsafe { core::mem::transmute(val) }),
            13 => BkeyValSC::quota(k, unsafe { core::mem::transmute(val) }),
            14 => BkeyValSC::stripe(k, unsafe { core::mem::transmute(val) }),
            15 => BkeyValSC::reflink_p(k, unsafe { core::mem::transmute(val) }),
            16 => BkeyValSC::reflink_v(k, unsafe { core::mem::transmute(val) }),
            17 => BkeyValSC::inline_data(k, unsafe { core::mem::transmute(val) }),
            18 => BkeyValSC::btree_ptr_v2(k, unsafe { core::mem::transmute(val) }),
            19 => BkeyValSC::indirect_inline_data(k, unsafe { core::mem::transmute(val) }),
            20 => BkeyValSC::alloc_v2(k, unsafe { core::mem::transmute(val) }),
            21 => BkeyValSC::subvolume(k, unsafe { core::mem::transmute(val) }),
            22 => BkeyValSC::snapshot(k, unsafe { core::mem::transmute(val) }),
            23 => BkeyValSC::inode_v2(k, unsafe { core::mem::transmute(val) }),
            24 => BkeyValSC::alloc_v3(k, unsafe { core::mem::transmute(val) }),
            25 => BkeyValSC::set(k, unsafe { core::mem::transmute(val) }),
            26 => BkeyValSC::lru(k, unsafe { core::mem::transmute(val) }),
            27 => BkeyValSC::alloc_v4(k, unsafe { core::mem::transmute(val) }),
            28 => BkeyValSC::backpointer(k, unsafe { core::mem::transmute(val) }),
            29 => BkeyValSC::inode_v3(k, unsafe { core::mem::transmute(val) }),
            30 => BkeyValSC::bucket_gens(k, unsafe { core::mem::transmute(val) }),
            31 => BkeyValSC::snapshot_tree(k, unsafe { core::mem::transmute(val) }),
            32 => BkeyValSC::logged_op_truncate(k, unsafe { core::mem::transmute(val) }),
            33 => BkeyValSC::logged_op_finsert(k, unsafe { core::mem::transmute(val) }),
            34 => BkeyValSC::accounting(k, unsafe { core::mem::transmute(val) }),
            35 => BkeyValSC::inode_alloc_cursor(k, unsafe { core::mem::transmute(val) }),
            36 => BkeyValSC::extent_whiteout(k, unsafe { core::mem::transmute(val) }),
            37 => BkeyValSC::logged_op_stripe_update(k, unsafe { core::mem::transmute(val) }),
            _ => BkeyValSC::unknown(k, k.type_),
        }
    }
}

/// Typed dispatch for split-mutable bkey references.
pub enum BkeyValS<'a> {
    deleted(&'a mut c::bkey, &'a mut c::bch_deleted),
    whiteout(&'a mut c::bkey, &'a mut c::bch_whiteout),
    error(&'a mut c::bkey, &'a mut c::bch_error),
    cookie(&'a mut c::bkey, &'a mut c::bch_cookie),
    hash_whiteout(&'a mut c::bkey, &'a mut c::bch_hash_whiteout),
    btree_ptr(&'a mut c::bkey, &'a mut c::bch_btree_ptr),
    extent(&'a mut c::bkey, &'a mut c::bch_extent),
    reservation(&'a mut c::bkey, &'a mut c::bch_reservation),
    inode(&'a mut c::bkey, &'a mut c::bch_inode),
    inode_generation(&'a mut c::bkey, &'a mut c::bch_inode_generation),
    dirent(&'a mut c::bkey, &'a mut c::bch_dirent),
    xattr(&'a mut c::bkey, &'a mut c::bch_xattr),
    alloc(&'a mut c::bkey, &'a mut c::bch_alloc),
    quota(&'a mut c::bkey, &'a mut c::bch_quota),
    stripe(&'a mut c::bkey, &'a mut c::bch_stripe),
    reflink_p(&'a mut c::bkey, &'a mut c::bch_reflink_p),
    reflink_v(&'a mut c::bkey, &'a mut c::bch_reflink_v),
    inline_data(&'a mut c::bkey, &'a mut c::bch_inline_data),
    btree_ptr_v2(&'a mut c::bkey, &'a mut c::bch_btree_ptr_v2),
    indirect_inline_data(&'a mut c::bkey, &'a mut c::bch_indirect_inline_data),
    alloc_v2(&'a mut c::bkey, &'a mut c::bch_alloc_v2),
    subvolume(&'a mut c::bkey, &'a mut c::bch_subvolume),
    snapshot(&'a mut c::bkey, &'a mut c::bch_snapshot),
    inode_v2(&'a mut c::bkey, &'a mut c::bch_inode_v2),
    alloc_v3(&'a mut c::bkey, &'a mut c::bch_alloc_v3),
    set(&'a mut c::bkey, &'a mut c::bch_set),
    lru(&'a mut c::bkey, &'a mut c::bch_lru),
    alloc_v4(&'a mut c::bkey, &'a mut c::bch_alloc_v4),
    backpointer(&'a mut c::bkey, &'a mut c::bch_backpointer),
    inode_v3(&'a mut c::bkey, &'a mut c::bch_inode_v3),
    bucket_gens(&'a mut c::bkey, &'a mut c::bch_bucket_gens),
    snapshot_tree(&'a mut c::bkey, &'a mut c::bch_snapshot_tree),
    logged_op_truncate(&'a mut c::bkey, &'a mut c::bch_logged_op_truncate),
    logged_op_finsert(&'a mut c::bkey, &'a mut c::bch_logged_op_finsert),
    accounting(&'a mut c::bkey, &'a mut c::bch_accounting),
    inode_alloc_cursor(&'a mut c::bkey, &'a mut c::bch_inode_alloc_cursor),
    extent_whiteout(&'a mut c::bkey, &'a mut c::bch_extent_whiteout),
    logged_op_stripe_update(&'a mut c::bkey, &'a mut c::bch_logged_op_stripe_update),
    unknown(&'a mut c::bkey, u8),
}

impl<'a> BkeyValS<'a> {
    #[allow(clippy::missing_transmute_annotations)]
    pub fn from_bkey_i(k: &'a mut c::bkey_i) -> Self {
        let type_ = k.k.type_;
        match type_ as u32 {
            0 => BkeyValS::deleted(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            1 => BkeyValS::whiteout(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            2 => BkeyValS::error(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            3 => BkeyValS::cookie(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            4 => BkeyValS::hash_whiteout(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            5 => BkeyValS::btree_ptr(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            6 => BkeyValS::extent(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            7 => BkeyValS::reservation(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            8 => BkeyValS::inode(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            9 => BkeyValS::inode_generation(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            10 => BkeyValS::dirent(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            11 => BkeyValS::xattr(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            12 => BkeyValS::alloc(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            13 => BkeyValS::quota(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            14 => BkeyValS::stripe(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            15 => BkeyValS::reflink_p(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            16 => BkeyValS::reflink_v(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            17 => BkeyValS::inline_data(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            18 => BkeyValS::btree_ptr_v2(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            19 => BkeyValS::indirect_inline_data(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            20 => BkeyValS::alloc_v2(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            21 => BkeyValS::subvolume(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            22 => BkeyValS::snapshot(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            23 => BkeyValS::inode_v2(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            24 => BkeyValS::alloc_v3(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            25 => BkeyValS::set(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            26 => BkeyValS::lru(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            27 => BkeyValS::alloc_v4(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            28 => BkeyValS::backpointer(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            29 => BkeyValS::inode_v3(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            30 => BkeyValS::bucket_gens(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            31 => BkeyValS::snapshot_tree(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            32 => BkeyValS::logged_op_truncate(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            33 => BkeyValS::logged_op_finsert(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            34 => BkeyValS::accounting(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            35 => BkeyValS::inode_alloc_cursor(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            36 => BkeyValS::extent_whiteout(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            37 => BkeyValS::logged_op_stripe_update(&mut k.k, unsafe { core::mem::transmute(&mut k.v) }),
            _ => BkeyValS::unknown(&mut k.k, type_),
        }
    }
}
