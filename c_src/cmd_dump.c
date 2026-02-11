#include <fcntl.h>
#include <getopt.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>

#include "cmds.h"
#include "libbcachefs.h"
#include "qcow2.h"

#include "bcachefs.h"
#include "btree/cache.h"
#include "btree/iter.h"
#include "btree/read.h"

#include "data/extents.h"

#include "init/error.h"
#include "init/fs.h"

#include "journal/read.h"

#include "sb/members.h"

struct dump_dev {
	ranges	sb, journal, btree;
};
typedef DARRAY(struct dump_dev) dump_devs;

static void dump_node(struct bch_fs *c, dump_devs *devs, struct bkey_s_c k)
{
	struct bkey_ptrs_c ptrs = bch2_bkey_ptrs_c(k);
#if 0
	unsigned bytes = btree_ptr_sectors_written(k) << 9 ?: c->opts.btree_node_size;
#else
	/* Less fragile: */
	unsigned bytes = c->opts.btree_node_size;
#endif

	bkey_for_each_ptr(ptrs, ptr)
		if (bch2_dev_exists(c, ptr->dev))
			range_add(&devs->data[ptr->dev].btree,
				  ptr->offset << 9, bytes);
}

static void get_sb_journal(struct bch_fs *c, struct bch_dev *ca,
			    bool entire_journal,
			    struct dump_dev *d)
{
	struct bch_sb *sb = ca->disk_sb.sb;

	/* Superblock: */
	range_add(&d->sb, BCH_SB_LAYOUT_SECTOR << 9,
		  sizeof(struct bch_sb_layout));

	for (unsigned i = 0; i < sb->layout.nr_superblocks; i++)
		range_add(&d->sb,
			  le64_to_cpu(sb->layout.sb_offset[i]) << 9,
			  vstruct_bytes(sb));

	/* Journal: */
	for (unsigned i = 0; i < ca->journal.nr; i++)
		if (entire_journal ||
		    ca->journal.bucket_seq[i] >= c->journal.last_seq_ondisk) {
			u64 bucket = ca->journal.buckets[i];

			range_add(&d->journal,
				  bucket_bytes(ca) * bucket,
				  bucket_bytes(ca));
		}
}

struct dump_opts {
	char		*out;
	bool		force;
	bool		sanitize;
	bool		sanitize_filenames;
	bool		entire_journal;
	bool		noexcl;
};

static void sanitize_key(struct bkey_packed *k, struct bkey_format *f, void *end,
			 struct dump_opts opts, bool *modified)
{
	struct bch_val *v = bkeyp_val(f, k);
	unsigned len = min_t(unsigned, end - (void *) v, bkeyp_val_bytes(f, k));

	switch (k->type) {
	case KEY_TYPE_inline_data: {
		struct bch_inline_data *d = container_of(v, struct bch_inline_data, v);

		memset(&d->data[0], 0, len - offsetof(struct bch_inline_data, data));
		*modified = true;
		break;
	}
	case KEY_TYPE_indirect_inline_data: {
		struct bch_indirect_inline_data *d = container_of(v, struct bch_indirect_inline_data, v);

		memset(&d->data[0], 0, len - offsetof(struct bch_indirect_inline_data, data));
		*modified = true;
		break;
	}

	case KEY_TYPE_dirent:
		if (opts.sanitize_filenames) {
			struct bch_dirent *d = container_of(v, struct bch_dirent, v);

			memset(d->d_name, 'X', len - offsetof(struct bch_dirent, d_name));
			*modified = true;
		}
	}
}

static void sanitize_journal(struct bch_fs *c, void *buf, size_t len,
			     struct dump_opts opts)
{
	struct bkey_format f = BKEY_FORMAT_CURRENT;
	void *end = buf + len;

	while (len) {
		struct jset *j = buf;
		bool modified = false;

		if (le64_to_cpu(j->magic) != jset_magic(c))
			break;

		if (bch2_csum_type_is_encryption(JSET_CSUM_TYPE(j))) {
			if (!c->chacha20_key_set) {
				fprintf(stderr,
					"found encrypted journal entry on non-encrypted filesystem\n");
				return;
			}

			if (vstruct_bytes(j) > len) {
				fprintf(stderr,
					"encrypted journal entry overruns bucket; skipping\n");
				return;
			}

			int ret = bch2_encrypt(c, JSET_CSUM_TYPE(j), journal_nonce(j),
					       j->encrypted_start,
					       vstruct_end(j) - (void *) j->encrypted_start);
			if (ret)
				die("error deecrypting journal entry: %s", bch2_err_str(ret));

			modified = true;
		}

		vstruct_for_each(j, i) {
			if ((void *) i >= end)
				break;

			if (!jset_entry_is_key(i))
				continue;

			jset_entry_for_each_key(i, k) {
				if ((void *) k >= end)
					break;
				if (!k->k.u64s)
					break;
				sanitize_key(bkey_to_packed(k), &f, end, opts, &modified);
			}
		}

		if (modified) {
			memset(&j->csum, 0, sizeof(j->csum));
			SET_JSET_CSUM_TYPE(j, 0);
		}

		unsigned b = min(len, vstruct_sectors(j, c->block_bits) << 9);
		len -= b;
		buf += b;
	}
}

static void sanitize_btree(struct bch_fs *c, void *buf, size_t len,
			   struct dump_opts opts)
{
	void *end = buf + len;
	bool first = true;
	struct bkey_format f_current = BKEY_FORMAT_CURRENT;
	struct bkey_format f;
	unsigned offset = 0;
	u64 seq;

	while (len) {
		unsigned sectors;
		struct bset *i;
		bool modified = false;

		if (first) {
			struct btree_node *bn = buf;

			if (le64_to_cpu(bn->magic) != bset_magic(c))
				break;

			i = &bn->keys;
			seq = bn->keys.seq;
			f = bn->format;

			sectors = vstruct_sectors(bn, c->block_bits);
		} else {
			struct btree_node_entry *bne = buf;

			if (bne->keys.seq != seq)
				break;

			i = &bne->keys;
			sectors = vstruct_sectors(bne, c->block_bits);
		}

		if (bch2_csum_type_is_encryption(BSET_CSUM_TYPE(i))) {
			if (!c->chacha20_key_set) {
				fprintf(stderr,
					"found encrypted btree node on non-encrypted filesystem\n");
				return;
			}

			if (vstruct_end(i) > end) {
				fprintf(stderr,
					"encrypted btree node entry overruns bucket; skipping\n");
				return;
			}

			int ret = bset_encrypt(c, i, offset);
			if (ret)
				die("error deecrypting btree node: %s", bch2_err_str(ret));

			modified = true;
		}

		vstruct_for_each(i, k) {
			if ((void *) k >= end)
				break;
			if (!k->u64s)
				break;

			sanitize_key(k, bkey_packed(k) ? &f : &f_current, end, opts, &modified);
		}

		if (modified) {
			if (first) {
				struct btree_node *bn = buf;
				memset(&bn->csum, 0, sizeof(bn->csum));
			} else {
				struct btree_node_entry *bne = buf;
				memset(&bne->csum, 0, sizeof(bne->csum));
			}
			SET_BSET_CSUM_TYPE(i, 0);
		}

		first = false;

		unsigned b = min(len, sectors << 9);
		len -= b;
		buf += b;
		offset += b;
	}
}

static int dump_fs(struct bch_fs *c, struct dump_opts opts)
{
	if (opts.sanitize_filenames)
		printf("Sanitizing filenames inline data extents\n");
	else if (opts.sanitize)
		printf("Sanitizing inline data extents\n");

	dump_devs devs = {};
	while (devs.nr < c->sb.nr_devices)
		darray_push(&devs, (struct dump_dev) {});

	down_read(&c->state_lock);

	unsigned nr_online = 0;
	for_each_online_member(c, ca, 0) {
		if (opts.sanitize && ca->mi.bucket_size % block_sectors(c))
			die("%s has unaligned buckets, cannot sanitize", ca->name);

		get_sb_journal(c, ca, opts.entire_journal, &devs.data[ca->dev_idx]);
		nr_online++;
	}

	bch_verbose(c, "walking metadata to dump");
	for (unsigned i = 0; i < BTREE_ID_NR; i++) {
		CLASS(btree_trans, trans)(c);

		int ret = __for_each_btree_node(trans, iter, i, POS_MIN, 0, 1, 0, b, ({
			struct btree_node_iter iter;
			struct bkey u;
			struct bkey_s_c k;

			for_each_btree_node_key_unpack(b, k, &iter, &u)
				dump_node(c, &devs, k);
			0;
		}));

		if (ret)
			die("error %s walking btree nodes", bch2_err_str(ret));

		struct btree *b = bch2_btree_id_root(c, i)->b;
		if (!btree_node_fake(b))
			dump_node(c, &devs, bkey_i_to_s_c(&b->key));
	}

	bch_verbose(c, "writing metadata image(s)");
	for_each_online_member(c, ca, 0) {
		int flags = O_WRONLY|O_CREAT|O_TRUNC;

		if (!opts.force)
			flags |= O_EXCL;

		char *path = nr_online > 1
			? mprintf("%s.%u.qcow2", opts.out, ca->dev_idx)
			: mprintf("%s.qcow2", opts.out);
		int fd = xopen(path, flags, 0600);
		free(path);

		struct qcow2_image img;
		qcow2_image_init(&img, ca->disk_sb.bdev->bd_fd, fd, c->opts.block_size);

		struct dump_dev *d = &devs.data[ca->dev_idx];

		qcow2_write_ranges(&img, &d->sb);

		if (!opts.sanitize) {
			qcow2_write_ranges(&img, &d->journal);
			qcow2_write_ranges(&img, &d->btree);
		} else {
			ranges_sort(&d->journal);
			ranges_sort(&d->btree);

			u64 bucket_bytes = ca->mi.bucket_size << 9;
			char *buf = xmalloc(bucket_bytes);

			darray_for_each(d->journal, r) {
				u64 len = r->end - r->start;
				BUG_ON(len > bucket_bytes);

				xpread(img.infd, buf, len, r->start);
				sanitize_journal(c, buf, len, opts);
				qcow2_write_buf(&img, buf, len, r->start);
			}

			darray_for_each(d->btree, r) {
				u64 len = r->end - r->start;
				BUG_ON(len > bucket_bytes);

				xpread(img.infd, buf, len, r->start);
				sanitize_btree(c, buf, len, opts);
				qcow2_write_buf(&img, buf, len, r->start);
			}
			free(buf);
		}

		qcow2_image_finish(&img);
		xclose(fd);
	}

	up_read(&c->state_lock);

	bch2_fs_exit(c);

	darray_for_each(devs, d) {
		darray_exit(&d->sb);
		darray_exit(&d->journal);
		darray_exit(&d->btree);
	}
	darray_exit(&devs);
	return 0;
}

static void dump_usage(void)
{
	puts("bcachefs dump - dump filesystem metadata\n"
	     "Usage: bcachefs dump [OPTION]... <devices>\n"
	     "\n"
	     "Options:\n"
	     "  -o output                     Output qcow2 image(s)\n"
	     "  -f, --force                   Force; overwrite when needed\n"
	     "  -s, --sanitize=data,filenames Zero out inline data extents, or filenames and inline data\n"
	     "      --nojournal               Don't dump entire journal, just dirty entries\n"
	     "      --noexcl                  Open devices with O_NOEXCL (not recommended)\n"
	     "  -v, --verbose\n"
	     "  -h, --help                    Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

int cmd_dump(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "force",		no_argument,		NULL, 'f' },
		{ "sanitize",		optional_argument,	NULL, 's' },
		{ "nojournal",		no_argument,		NULL, 'j' },
		{ "noexcl",		no_argument,		NULL, 'e' },
		{ "verbose",		no_argument,		NULL, 'v' },
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};
	struct bch_opts fs_opts = bch2_opts_empty();
	struct dump_opts opts = { .entire_journal = true };
	int opt;

	opt_set(fs_opts, direct_io,	false);
	opt_set(fs_opts, read_only,	true);
	opt_set(fs_opts, nochanges,	true);
	opt_set(fs_opts, norecovery,	true);
	opt_set(fs_opts, degraded,	BCH_DEGRADED_very);
	opt_set(fs_opts, errors,	BCH_ON_ERROR_continue);
	opt_set(fs_opts, fix_errors,	FSCK_FIX_no);

	while ((opt = getopt_long(argc, argv, "o:fsvh",
				  longopts, NULL)) != -1)
		switch (opt) {
		case 'o':
			opts.out = optarg;
			break;
		case 'f':
			opts.force = true;
			break;
		case 's':
			opts.sanitize = true;

			if (optarg) {
				if (!strcmp(optarg, "data"))
					;
				else if (!strcmp(optarg, "filenames"))
					opts.sanitize_filenames = true;
				else
					die("Bad sanitize option %s", optarg);
			}
			break;
		case 'j':
			opts.entire_journal = false;
			break;
		case 'e':
			opt_set(fs_opts, noexcl,	true);
			break;
		case 'v':
			opt_set(fs_opts, verbose, true);
			break;
		case 'h':
			dump_usage();
			exit(EXIT_SUCCESS);
		}
	args_shift(optind);

	if (!opts.out)
		die("Please supply output filename");

	if (!argc) {
		dump_usage();
		die("Please supply device(s) to check");
	}

	darray_const_str dev_names = get_or_split_cmdline_devs(argc, argv);

	struct bch_fs *c = bch2_fs_open(&dev_names, &fs_opts);
	if (IS_ERR(c))
		die("error opening devices: %s", bch2_err_str(PTR_ERR(c)));

	int ret = dump_fs(c, opts);
	darray_exit(&dev_names);
	return ret;
}

