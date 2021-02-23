#include <fcntl.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>

#include "cmds.h"
#include "libbcachefs.h"
#include "qcow2.h"
#include "tools-util.h"

#include "libbcachefs/bcachefs.h"
#include "libbcachefs/bset.h"
#include "libbcachefs/btree_cache.h"
#include "libbcachefs/btree_io.h"
#include "libbcachefs/btree_iter.h"
#include "libbcachefs/buckets.h"
#include "libbcachefs/checksum.h"
#include "libbcachefs/error.h"
#include "libbcachefs/journal.h"
#include "libbcachefs/journal_io.h"
#include "libbcachefs/super.h"

static void dump_usage(void)
{
	puts("bcachefs dump - dump filesystem metadata\n"
	     "Usage: bcachefs dump [OPTION]... <devices>\n"
	     "\n"
	     "Options:\n"
	     "  -o output     Output qcow2 image(s)\n"
	     "  -f            Force; overwrite when needed\n"
	     "  -h            Display this help and exit\n"
	     "Report bugs to <linux-bcache@vger.kernel.org>");
}

static void dump_one_device(struct bch_fs *c, struct bch_dev *ca, int fd)
{
	struct bch_sb *sb = ca->disk_sb.sb;
	ranges data;
	unsigned i;

	darray_init(data);

	/* Superblock: */
	range_add(&data, BCH_SB_LAYOUT_SECTOR << 9,
		  sizeof(struct bch_sb_layout));

	for (i = 0; i < sb->layout.nr_superblocks; i++)
		range_add(&data,
			  le64_to_cpu(sb->layout.sb_offset[i]) << 9,
			  vstruct_bytes(sb));

	/* Journal: */
	for (i = 0; i < ca->journal.nr; i++)
		if (ca->journal.bucket_seq[i] >= c->journal.last_seq_ondisk) {
			u64 bucket = ca->journal.buckets[i];

			range_add(&data,
				  bucket_bytes(ca) * bucket,
				  bucket_bytes(ca));
		}

	/* Btree: */
	for (i = 0; i < BTREE_ID_NR; i++) {
		const struct bch_extent_ptr *ptr;
		struct bkey_ptrs_c ptrs;
		struct btree_trans trans;
		struct btree_iter *iter;
		struct btree *b;

		bch2_trans_init(&trans, c, 0, 0);

		__for_each_btree_node(&trans, iter, i, POS_MIN, 0, 1, 0, b) {
			struct btree_node_iter iter;
			struct bkey u;
			struct bkey_s_c k;

			for_each_btree_node_key_unpack(b, k, &iter, &u) {
				ptrs = bch2_bkey_ptrs_c(k);

				bkey_for_each_ptr(ptrs, ptr)
					if (ptr->dev == ca->dev_idx)
						range_add(&data,
							  ptr->offset << 9,
							  btree_bytes(c));
			}
		}

		b = c->btree_roots[i].b;
		if (!btree_node_fake(b)) {
			ptrs = bch2_bkey_ptrs_c(bkey_i_to_s_c(&b->key));

			bkey_for_each_ptr(ptrs, ptr)
				if (ptr->dev == ca->dev_idx)
					range_add(&data,
						  ptr->offset << 9,
						  btree_bytes(c));
		}
		bch2_trans_exit(&trans);
	}

	qcow2_write_image(ca->disk_sb.bdev->bd_fd, fd, &data,
			  max_t(unsigned, btree_bytes(c) / 8, block_bytes(c)));
}

int cmd_dump(int argc, char *argv[])
{
	struct bch_opts opts = bch2_opts_empty();
	struct bch_dev *ca;
	char *out = NULL;
	unsigned i, nr_devices = 0;
	bool force = false;
	int fd, opt;

	opt_set(opts, nochanges,	true);
	opt_set(opts, norecovery,	true);
	opt_set(opts, degraded,		true);
	opt_set(opts, errors,		BCH_ON_ERROR_CONTINUE);
	opt_set(opts, fix_errors,	FSCK_OPT_YES);

	while ((opt = getopt(argc, argv, "o:fvh")) != -1)
		switch (opt) {
		case 'o':
			out = optarg;
			break;
		case 'f':
			force = true;
			break;
		case 'v':
			opt_set(opts, verbose, true);
			break;
		case 'h':
			dump_usage();
			exit(EXIT_SUCCESS);
		}
	args_shift(optind);

	if (!out)
		die("Please supply output filename");

	if (!argc)
		die("Please supply device(s) to check");

	struct bch_fs *c = bch2_fs_open(argv, argc, opts);
	if (IS_ERR(c))
		die("error opening %s: %s", argv[0], strerror(-PTR_ERR(c)));

	down_read(&c->gc_lock);

	for_each_online_member(ca, c, i)
		nr_devices++;

	BUG_ON(!nr_devices);

	for_each_online_member(ca, c, i) {
		int flags = O_WRONLY|O_CREAT|O_TRUNC;

		if (!force)
			flags |= O_EXCL;

		if (!c->devs[i])
			continue;

		char *path = nr_devices > 1
			? mprintf("%s.%u", out, i)
			: strdup(out);
		fd = xopen(path, flags, 0600);
		free(path);

		dump_one_device(c, ca, fd);
		close(fd);
	}

	up_read(&c->gc_lock);

	bch2_fs_stop(c);
	return 0;
}

static void list_keys(struct bch_fs *c, enum btree_id btree_id,
		      struct bpos start, struct bpos end)
{
	struct btree_trans trans;
	struct btree_iter *iter;
	struct bkey_s_c k;
	char buf[512];
	int ret;

	bch2_trans_init(&trans, c, 0, 0);

	for_each_btree_key(&trans, iter, btree_id, start,
			   BTREE_ITER_PREFETCH, k, ret) {
		if (bkey_cmp(k.k->p, end) > 0)
			break;

		bch2_bkey_val_to_text(&PBUF(buf), c, k);
		puts(buf);
	}
	bch2_trans_exit(&trans);
}

static void list_btree_formats(struct bch_fs *c, enum btree_id btree_id,
			       struct bpos start, struct bpos end)
{
	struct btree_trans trans;
	struct btree_iter *iter;
	struct btree *b;
	char buf[4096];

	bch2_trans_init(&trans, c, 0, 0);

	for_each_btree_node(&trans, iter, btree_id, start, 0, b) {
		if (bkey_cmp(b->key.k.p, end) > 0)
			break;

		bch2_btree_node_to_text(&PBUF(buf), c, b);
		puts(buf);
	}
	bch2_trans_exit(&trans);
}

static void list_nodes(struct bch_fs *c, enum btree_id btree_id,
			    struct bpos start, struct bpos end)
{
	struct btree_trans trans;
	struct btree_iter *iter;
	struct btree *b;
	char buf[4096];

	bch2_trans_init(&trans, c, 0, 0);

	for_each_btree_node(&trans, iter, btree_id, start, 0, b) {
		if (bkey_cmp(b->key.k.p, end) > 0)
			break;

		bch2_bkey_val_to_text(&PBUF(buf), c, bkey_i_to_s_c(&b->key));
		fputs(buf, stdout);
		putchar('\n');
	}
	bch2_trans_exit(&trans);
}

static void print_node_ondisk(struct bch_fs *c, struct btree *b)
{
	struct btree_node *n_ondisk;
	struct extent_ptr_decoded pick;
	struct bch_dev *ca;
	struct bio *bio;
	unsigned offset = 0;

	if (bch2_bkey_pick_read_device(c, bkey_i_to_s_c(&b->key), NULL, &pick) <= 0) {
		printf("error getting device to read from\n");
		return;
	}

	ca = bch_dev_bkey_exists(c, pick.ptr.dev);
	if (!bch2_dev_get_ioref(ca, READ)) {
		printf("error getting device to read from\n");
		return;
	}

	n_ondisk = malloc(btree_bytes(c));

	bio = bio_alloc_bioset(GFP_NOIO,
			buf_pages(n_ondisk, btree_bytes(c)),
			&c->btree_bio);
	bio_set_dev(bio, ca->disk_sb.bdev);
	bio->bi_opf		= REQ_OP_READ|REQ_META;
	bio->bi_iter.bi_sector	= pick.ptr.offset;
	bch2_bio_map(bio, n_ondisk, btree_bytes(c));

	submit_bio_wait(bio);

	bio_put(bio);
	percpu_ref_put(&ca->io_ref);

	while (offset < c->opts.btree_node_size) {
		struct bset *i;
		struct nonce nonce;
		struct bch_csum csum;
		struct bkey_packed *k;
		unsigned sectors;

		if (!offset) {
			i = &n_ondisk->keys;

			if (!bch2_checksum_type_valid(c, BSET_CSUM_TYPE(i)))
				die("unknown checksum type");

			nonce = btree_nonce(i, offset << 9);
			csum = csum_vstruct(c, BSET_CSUM_TYPE(i), nonce, n_ondisk);

			if (bch2_crc_cmp(csum, n_ondisk->csum))
				die("invalid checksum\n");

			bset_encrypt(c, i, offset << 9);

			sectors = vstruct_sectors(n_ondisk, c->block_bits);
		} else {
			struct btree_node_entry *bne = (void *) n_ondisk + (offset << 9);

			i = &bne->keys;

			if (i->seq != n_ondisk->keys.seq)
				break;

			if (!bch2_checksum_type_valid(c, BSET_CSUM_TYPE(i)))
				die("unknown checksum type");

			nonce = btree_nonce(i, offset << 9);
			csum = csum_vstruct(c, BSET_CSUM_TYPE(i), nonce, bne);

			if (bch2_crc_cmp(csum, bne->csum))
				die("invalid checksum");

			bset_encrypt(c, i, offset << 9);

			sectors = vstruct_sectors(bne, c->block_bits);
		}

		fprintf(stdout, "  offset %u version %u, journal seq %llu\n",
			offset,
			le16_to_cpu(i->version),
			le64_to_cpu(i->journal_seq));
		offset += sectors;

		for (k = i->start;
		     k != vstruct_last(i);
		     k = bkey_next_skip_noops(k, vstruct_last(i))) {
			struct bkey u;
			char buf[4096];

			bch2_bkey_val_to_text(&PBUF(buf), c, bkey_disassemble(b, k, &u));
			fprintf(stdout, "    %s\n", buf);
		}
	}

	free(n_ondisk);
}

static void list_nodes_ondisk(struct bch_fs *c, enum btree_id btree_id,
			      struct bpos start, struct bpos end)
{
	struct btree_trans trans;
	struct btree_iter *iter;
	struct btree *b;
	char buf[4096];

	bch2_trans_init(&trans, c, 0, 0);

	for_each_btree_node(&trans, iter, btree_id, start, 0, b) {
		if (bkey_cmp(b->key.k.p, end) > 0)
			break;

		bch2_bkey_val_to_text(&PBUF(buf), c, bkey_i_to_s_c(&b->key));
		fputs(buf, stdout);
		putchar('\n');

		print_node_ondisk(c, b);
	}
	bch2_trans_exit(&trans);
}

static void list_nodes_keys(struct bch_fs *c, enum btree_id btree_id,
			    struct bpos start, struct bpos end)
{
	struct btree_trans trans;
	struct btree_iter *iter;
	struct btree_node_iter node_iter;
	struct bkey unpacked;
	struct bkey_s_c k;
	struct btree *b;
	char buf[4096];

	bch2_trans_init(&trans, c, 0, 0);

	for_each_btree_node(&trans, iter, btree_id, start, 0, b) {
		if (bkey_cmp(b->key.k.p, end) > 0)
			break;

		bch2_btree_node_to_text(&PBUF(buf), c, b);
		fputs(buf, stdout);

		for_each_btree_node_key_unpack(b, k, &node_iter, &unpacked) {
			bch2_bkey_val_to_text(&PBUF(buf), c, k);
			putchar('\t');
			puts(buf);
		}
	}
	bch2_trans_exit(&trans);
}

static struct bpos parse_pos(char *buf)
{
	char *s = buf, *field;
	u64 inode_v = 0, offset_v = 0;

	if (!(field = strsep(&s, ":")) ||
	    kstrtoull(field, 10, &inode_v))
		die("invalid bpos %s", buf);

	if ((field = strsep(&s, ":")) &&
	    kstrtoull(field, 10, &offset_v))
		die("invalid bpos %s", buf);

	if (s)
		die("invalid bpos %s", buf);

	return (struct bpos) { .inode = inode_v, .offset = offset_v };
}

static void list_keys_usage(void)
{
	puts("bcachefs list - list filesystem metadata to stdout\n"
	     "Usage: bcachefs list [OPTION]... <devices>\n"
	     "\n"
	     "Options:\n"
	     "  -b (extents|inodes|dirents|xattrs)    Btree to list from\n"
	     "  -s inode:offset                       Start position to list from\n"
	     "  -e inode:offset                       End position\n"
	     "  -i inode                              List keys for a given inode number\n"
	     "  -m (keys|formats)                     List mode\n"
	     "  -f                                    Check (fsck) the filesystem first\n"
	     "  -v                                    Verbose mode\n"
	     "  -h                                    Display this help and exit\n"
	     "Report bugs to <linux-bcache@vger.kernel.org>");
}

static const char * const list_modes[] = {
	"keys",
	"formats",
	"nodes",
	"nodes_ondisk",
	"nodes_keys",
	NULL
};

int cmd_list(int argc, char *argv[])
{
	struct bch_opts opts = bch2_opts_empty();
	enum btree_id btree_id_start	= 0;
	enum btree_id btree_id_end	= BTREE_ID_NR;
	enum btree_id btree_id;
	struct bpos start = POS_MIN, end = POS_MAX;
	u64 inum;
	int mode = 0, opt;

	opt_set(opts, nochanges,	true);
	opt_set(opts, norecovery,	true);
	opt_set(opts, degraded,		true);
	opt_set(opts, errors,		BCH_ON_ERROR_CONTINUE);

	while ((opt = getopt(argc, argv, "b:s:e:i:m:fvh")) != -1)
		switch (opt) {
		case 'b':
			btree_id_start = read_string_list_or_die(optarg,
						bch2_btree_ids, "btree id");
			btree_id_end = btree_id_start + 1;
			break;
		case 's':
			start	= parse_pos(optarg);
			break;
		case 'e':
			end	= parse_pos(optarg);
			break;
		case 'i':
			if (kstrtoull(optarg, 10, &inum))
				die("invalid inode %s", optarg);
			start	= POS(inum, 0);
			end	= POS(inum + 1, 0);
			break;
		case 'm':
			mode = read_string_list_or_die(optarg,
						list_modes, "list mode");
			break;
		case 'f':
			opt_set(opts, fix_errors, FSCK_OPT_YES);
			opt_set(opts, norecovery, false);
			break;
		case 'v':
			opt_set(opts, verbose, true);
			break;
		case 'h':
			list_keys_usage();
			exit(EXIT_SUCCESS);
		}
	args_shift(optind);

	if (!argc)
		die("Please supply device(s)");

	struct bch_fs *c = bch2_fs_open(argv, argc, opts);
	if (IS_ERR(c))
		die("error opening %s: %s", argv[0], strerror(-PTR_ERR(c)));


	for (btree_id = btree_id_start;
	     btree_id < btree_id_end;
	     btree_id++) {
		switch (mode) {
		case 0:
			list_keys(c, btree_id, start, end);
			break;
		case 1:
			list_btree_formats(c, btree_id, start, end);
			break;
		case 2:
			list_nodes(c, btree_id, start, end);
			break;
		case 3:
			list_nodes_ondisk(c, btree_id, start, end);
			break;
		case 4:
			list_nodes_keys(c, btree_id, start, end);
			break;
		default:
			die("Invalid mode");
		}
	}

	bch2_fs_stop(c);
	return 0;
}

static void list_journal_usage(void)
{
	puts("bcachefs list_journal - print contents of journal\n"
	     "Usage: bcachefs list_journal [OPTION]... <devices>\n"
	     "\n"
	     "Options:\n"
	     "  -a            Read entire journal, not just dirty entries\n"
	     "  -h            Display this help and exit\n"
	     "Report bugs to <linux-bcache@vger.kernel.org>");
}

int cmd_list_journal(int argc, char *argv[])
{
	struct bch_opts opts = bch2_opts_empty();
	int opt;

	opt_set(opts, nochanges,	true);
	opt_set(opts, norecovery,	true);
	opt_set(opts, degraded,		true);
	opt_set(opts, errors,		BCH_ON_ERROR_CONTINUE);
	opt_set(opts, fix_errors,	FSCK_OPT_YES);
	opt_set(opts, keep_journal,	true);

	while ((opt = getopt(argc, argv, "ah")) != -1)
		switch (opt) {
		case 'a':
			opt_set(opts, read_entire_journal, true);
			break;
		case 'h':
			list_journal_usage();
			exit(EXIT_SUCCESS);
		}
	args_shift(optind);

	if (!argc)
		die("Please supply device(s) to open");

	struct bch_fs *c = bch2_fs_open(argv, argc, opts);
	if (IS_ERR(c))
		die("error opening %s: %s", argv[0], strerror(-PTR_ERR(c)));

	struct journal_replay *p;
	struct jset_entry *entry;
	struct bkey_i *k, *_n;

	/* This could be greatly expanded: */

	list_for_each_entry(p, &c->journal_entries, list) {
		printf("journal entry   %8llu\n"
		       "    version     %8u\n"
		       "    last seq    %8llu\n"
		       ,
		       le64_to_cpu(p->j.seq),
		       le32_to_cpu(p->j.version),
		       le64_to_cpu(p->j.last_seq));

		for_each_jset_key(k, _n, entry, &p->j) {
			char buf[200];

			bch2_bkey_val_to_text(&PBUF(buf), c, bkey_i_to_s_c(k));
			printf("btree %s l %u: %s\n",
			       bch2_btree_ids[entry->btree_id],
			       entry->level,
			       buf);
		}
	}

	bch2_fs_stop(c);
	return 0;
}
