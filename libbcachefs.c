#include <ctype.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#include <uuid/uuid.h>

#include "libbcachefs.h"
#include "crypto.h"
#include "libbcachefs/bcachefs_format.h"
#include "libbcachefs/btree_cache.h"
#include "libbcachefs/checksum.h"
#include "libbcachefs/disk_groups.h"
#include "libbcachefs/journal_seq_blacklist.h"
#include "libbcachefs/opts.h"
#include "libbcachefs/replicas.h"
#include "libbcachefs/super-io.h"
#include "tools-util.h"

#define NSEC_PER_SEC	1000000000L

/* minimum size filesystem we can create, given a bucket size: */
static u64 min_size(unsigned bucket_size)
{
	return BCH_MIN_NR_NBUCKETS * bucket_size;
}

static void init_layout(struct bch_sb_layout *l, unsigned block_size,
			u64 start, u64 end)
{
	unsigned sb_size;
	u64 backup; /* offset of 2nd sb */

	memset(l, 0, sizeof(*l));

	if (start != BCH_SB_SECTOR)
		start = round_up(start, block_size);
	end = round_down(end, block_size);

	if (start >= end)
		die("insufficient space for superblocks");

	/*
	 * Create two superblocks in the allowed range: reserve a maximum of 64k
	 */
	sb_size = min_t(u64, 128, end - start / 2);

	backup = start + sb_size;
	backup = round_up(backup, block_size);

	backup = min(backup, end);

	sb_size = min(end - backup, backup- start);
	sb_size = rounddown_pow_of_two(sb_size);

	if (sb_size < 8)
		die("insufficient space for superblocks");

	l->magic		= BCACHE_MAGIC;
	l->layout_type		= 0;
	l->nr_superblocks	= 2;
	l->sb_max_size_bits	= ilog2(sb_size);
	l->sb_offset[0]		= cpu_to_le64(start);
	l->sb_offset[1]		= cpu_to_le64(backup);
}

void bch2_pick_bucket_size(struct bch_opts opts, struct dev_opts *dev)
{
	if (!dev->sb_offset) {
		dev->sb_offset	= BCH_SB_SECTOR;
		dev->sb_end	= BCH_SB_SECTOR + 256;
	}

	if (!dev->size)
		dev->size = get_size(dev->path, dev->fd) >> 9;

	if (!dev->bucket_size) {
		if (dev->size < min_size(opts.block_size))
			die("cannot format %s, too small (%llu sectors, min %llu)",
			    dev->path, dev->size, min_size(opts.block_size));

		/* Bucket size must be >= block size: */
		dev->bucket_size = opts.block_size;

		/* Bucket size must be >= btree node size: */
		if (opt_defined(opts, btree_node_size))
			dev->bucket_size = max_t(unsigned, dev->bucket_size,
						 opts.btree_node_size);

		/* Want a bucket size of at least 128k, if possible: */
		dev->bucket_size = max(dev->bucket_size, 256U);

		if (dev->size >= min_size(dev->bucket_size)) {
			unsigned scale = max(1,
					     ilog2(dev->size / min_size(dev->bucket_size)) / 4);

			scale = rounddown_pow_of_two(scale);

			/* max bucket size 1 mb */
			dev->bucket_size = min(dev->bucket_size * scale, 1U << 11);
		} else {
			do {
				dev->bucket_size /= 2;
			} while (dev->size < min_size(dev->bucket_size));
		}
	}

	dev->nbuckets	= dev->size / dev->bucket_size;

	if (dev->bucket_size < opts.block_size)
		die("Bucket size cannot be smaller than block size");

	if (opt_defined(opts, btree_node_size) &&
	    dev->bucket_size < opts.btree_node_size)
		die("Bucket size cannot be smaller than btree node size");

	if (dev->nbuckets < BCH_MIN_NR_NBUCKETS)
		die("Not enough buckets: %llu, need %u (bucket size %u)",
		    dev->nbuckets, BCH_MIN_NR_NBUCKETS, dev->bucket_size);

}

static unsigned parse_target(struct bch_sb_handle *sb,
			     struct dev_opts *devs, size_t nr_devs,
			     const char *s)
{
	struct dev_opts *i;
	int idx;

	if (!s)
		return 0;

	for (i = devs; i < devs + nr_devs; i++)
		if (!strcmp(s, i->path))
			return dev_to_target(i - devs);

	idx = bch2_disk_path_find(sb, s);
	if (idx >= 0)
		return group_to_target(idx);

	die("Invalid target %s", s);
	return 0;
}

struct bch_sb *bch2_format(struct bch_opt_strs	fs_opt_strs,
			   struct bch_opts	fs_opts,
			   struct format_opts	opts,
			   struct dev_opts	*devs,
			   size_t		nr_devs)
{
	struct bch_sb_handle sb = { NULL };
	struct dev_opts *i;
	struct bch_sb_field_members *mi;
	unsigned max_dev_block_size = 0;
	unsigned opt_id;

	for (i = devs; i < devs + nr_devs; i++)
		max_dev_block_size = max(max_dev_block_size,
					 get_blocksize(i->path, i->fd));

	/* calculate block size: */
	if (!opt_defined(fs_opts, block_size)) {
		opt_set(fs_opts, block_size, max_dev_block_size);
	} else if (fs_opts.block_size < max_dev_block_size)
		die("blocksize too small: %u, must be greater than device blocksize %u",
		    fs_opts.block_size, max_dev_block_size);

	/* calculate bucket sizes: */
	for (i = devs; i < devs + nr_devs; i++)
		bch2_pick_bucket_size(fs_opts, i);

	/* calculate btree node size: */
	if (!opt_defined(fs_opts, btree_node_size)) {
		/* 256k default btree node size */
		opt_set(fs_opts, btree_node_size, 512);

		for (i = devs; i < devs + nr_devs; i++)
			fs_opts.btree_node_size =
				min_t(unsigned, fs_opts.btree_node_size,
				      i->bucket_size);
	}

	if (!is_power_of_2(fs_opts.block_size))
		die("block size must be power of 2");

	if (!is_power_of_2(fs_opts.btree_node_size))
		die("btree node size must be power of 2");

	if (uuid_is_null(opts.uuid.b))
		uuid_generate(opts.uuid.b);

	if (bch2_sb_realloc(&sb, 0))
		die("insufficient memory");

	sb.sb->version		= le16_to_cpu(bcachefs_metadata_version_current);
	sb.sb->version_min	= le16_to_cpu(bcachefs_metadata_version_current);
	sb.sb->magic		= BCACHE_MAGIC;
	sb.sb->block_size	= cpu_to_le16(fs_opts.block_size);
	sb.sb->user_uuid	= opts.uuid;
	sb.sb->nr_devices	= nr_devs;

	uuid_generate(sb.sb->uuid.b);

	if (opts.label)
		memcpy(sb.sb->label,
		       opts.label,
		       min(strlen(opts.label), sizeof(sb.sb->label)));

	for (opt_id = 0;
	     opt_id < bch2_opts_nr;
	     opt_id++) {
		const struct bch_option *opt = &bch2_opt_table[opt_id];
		u64 v;

		if (opt->set_sb == SET_NO_SB_OPT)
			continue;

		v = bch2_opt_defined_by_id(&fs_opts, opt_id)
			? bch2_opt_get_by_id(&fs_opts, opt_id)
			: bch2_opt_get_by_id(&bch2_opts_default, opt_id);

		opt->set_sb(sb.sb, v);
	}

	SET_BCH_SB_ENCODED_EXTENT_MAX_BITS(sb.sb,
				ilog2(opts.encoded_extent_max));

	struct timespec now;
	if (clock_gettime(CLOCK_REALTIME, &now))
		die("error getting current time: %m");

	sb.sb->time_base_lo	= cpu_to_le64(now.tv_sec * NSEC_PER_SEC + now.tv_nsec);
	sb.sb->time_precision	= cpu_to_le32(1);

	/* Member info: */
	mi = bch2_sb_resize_members(&sb,
			(sizeof(*mi) + sizeof(struct bch_member) *
			 nr_devs) / sizeof(u64));

	for (i = devs; i < devs + nr_devs; i++) {
		struct bch_member *m = mi->members + (i - devs);

		uuid_generate(m->uuid.b);
		m->nbuckets	= cpu_to_le64(i->nbuckets);
		m->first_bucket	= 0;
		m->bucket_size	= cpu_to_le16(i->bucket_size);

		SET_BCH_MEMBER_REPLACEMENT(m,	CACHE_REPLACEMENT_LRU);
		SET_BCH_MEMBER_DISCARD(m,	i->discard);
		SET_BCH_MEMBER_DATA_ALLOWED(m,	i->data_allowed);
		SET_BCH_MEMBER_DURABILITY(m,	i->durability + 1);
	}

	/* Disk groups */
	for (i = devs; i < devs + nr_devs; i++) {
		struct bch_member *m = mi->members + (i - devs);
		int idx;

		if (!i->group)
			continue;

		idx = bch2_disk_path_find_or_create(&sb, i->group);
		if (idx < 0)
			die("error creating disk path: %s", idx);

		SET_BCH_MEMBER_GROUP(m,	idx + 1);
	}

	SET_BCH_SB_FOREGROUND_TARGET(sb.sb,
		parse_target(&sb, devs, nr_devs, fs_opt_strs.foreground_target));
	SET_BCH_SB_BACKGROUND_TARGET(sb.sb,
		parse_target(&sb, devs, nr_devs, fs_opt_strs.background_target));
	SET_BCH_SB_PROMOTE_TARGET(sb.sb,
		parse_target(&sb, devs, nr_devs, fs_opt_strs.promote_target));
	SET_BCH_SB_METADATA_TARGET(sb.sb,
		parse_target(&sb, devs, nr_devs, fs_opt_strs.metadata_target));

	/* Crypt: */
	if (opts.encrypted) {
		struct bch_sb_field_crypt *crypt =
			bch2_sb_resize_crypt(&sb, sizeof(*crypt) / sizeof(u64));

		bch_sb_crypt_init(sb.sb, crypt, opts.passphrase);
		SET_BCH_SB_ENCRYPTION_TYPE(sb.sb, 1);
	}

	for (i = devs; i < devs + nr_devs; i++) {
		sb.sb->dev_idx = i - devs;

		init_layout(&sb.sb->layout, fs_opts.block_size,
			    i->sb_offset, i->sb_end);

		if (i->sb_offset == BCH_SB_SECTOR) {
			/* Zero start of disk */
			static const char zeroes[BCH_SB_SECTOR << 9];

			xpwrite(i->fd, zeroes, BCH_SB_SECTOR << 9, 0);
		}

		bch2_super_write(i->fd, sb.sb);
		close(i->fd);
	}

	return sb.sb;
}

void bch2_super_write(int fd, struct bch_sb *sb)
{
	struct nonce nonce = { 0 };

	unsigned i;
	for (i = 0; i < sb->layout.nr_superblocks; i++) {
		sb->offset = sb->layout.sb_offset[i];

		if (sb->offset == BCH_SB_SECTOR) {
			/* Write backup layout */
			xpwrite(fd, &sb->layout, sizeof(sb->layout),
				BCH_SB_LAYOUT_SECTOR << 9);
		}

		sb->csum = csum_vstruct(NULL, BCH_SB_CSUM_TYPE(sb), nonce, sb);
		xpwrite(fd, sb, vstruct_bytes(sb),
			le64_to_cpu(sb->offset) << 9);
	}

	fsync(fd);
}

struct bch_sb *__bch2_super_read(int fd, u64 sector)
{
	struct bch_sb sb, *ret;

	xpread(fd, &sb, sizeof(sb), sector << 9);

	if (memcmp(&sb.magic, &BCACHE_MAGIC, sizeof(sb.magic)))
		die("not a bcachefs superblock");

	size_t bytes = vstruct_bytes(&sb);

	ret = malloc(bytes);

	xpread(fd, ret, bytes, sector << 9);

	return ret;
}

static unsigned get_dev_has_data(struct bch_sb *sb, unsigned dev)
{
	struct bch_sb_field_replicas *replicas;
	struct bch_replicas_entry *r;
	unsigned i, data_has = 0;

	replicas = bch2_sb_get_replicas(sb);

	if (replicas)
		for_each_replicas_entry(replicas, r)
			for (i = 0; i < r->nr_devs; i++)
				if (r->devs[i] == dev)
					data_has |= 1 << r->data_type;

	return data_has;
}

static int bch2_sb_get_target(struct bch_sb *sb, char *buf, size_t len, u64 v)
{
	struct target t = target_decode(v);
	int ret;

	switch (t.type) {
	case TARGET_NULL:
		return scnprintf(buf, len, "none");
	case TARGET_DEV: {
		struct bch_sb_field_members *mi = bch2_sb_get_members(sb);
		struct bch_member *m = mi->members + t.dev;

		if (bch2_dev_exists(sb, mi, t.dev)) {
			char uuid_str[40];

			uuid_unparse(m->uuid.b, uuid_str);

			ret = scnprintf(buf, len, "Device %u (%s)", t.dev,
				uuid_str);
		} else {
			ret = scnprintf(buf, len, "Bad device %u", t.dev);
		}

		break;
	}
	case TARGET_GROUP: {
		struct bch_sb_field_disk_groups *gi;
		gi = bch2_sb_get_disk_groups(sb);

		struct bch_disk_group *g = gi->entries + t.group;

		if (t.group < disk_groups_nr(gi) && !BCH_GROUP_DELETED(g)) {
			ret = scnprintf(buf, len, "Group %u (%.*s)", t.group,
				BCH_SB_LABEL_SIZE, g->label);
		} else {
			ret = scnprintf(buf, len, "Bad group %u", t.group);
		}
		break;
	}
	default:
		BUG();
	}

	return ret;
}

/* superblock printing: */

static void bch2_sb_print_layout(struct bch_sb *sb, enum units units)
{
	struct bch_sb_layout *l = &sb->layout;
	unsigned i;

	printf("  type:				%u\n"
	       "  superblock max size:		%s\n"
	       "  nr superblocks:		%u\n"
	       "  Offsets:			",
	       l->layout_type,
	       pr_units(1 << l->sb_max_size_bits, units),
	       l->nr_superblocks);

	for (i = 0; i < l->nr_superblocks; i++) {
		if (i)
			printf(", ");
		printf("%llu", le64_to_cpu(l->sb_offset[i]));
	}
	putchar('\n');
}

static void bch2_sb_print_journal(struct bch_sb *sb, struct bch_sb_field *f,
				  enum units units)
{
	struct bch_sb_field_journal *journal = field_to_type(f, journal);
	unsigned i, nr = bch2_nr_journal_buckets(journal);

	printf("  Buckets:			");
	for (i = 0; i < nr; i++) {
		if (i)
			putchar(' ');
		printf("%llu", le64_to_cpu(journal->buckets[i]));
	}
	putchar('\n');
}

static void bch2_sb_print_members(struct bch_sb *sb, struct bch_sb_field *f,
				  enum units units)
{
	struct bch_sb_field_members *mi = field_to_type(f, members);
	struct bch_sb_field_disk_groups *gi = bch2_sb_get_disk_groups(sb);
	unsigned i;

	for (i = 0; i < sb->nr_devices; i++) {
		struct bch_member *m = mi->members + i;
		time_t last_mount = le64_to_cpu(m->last_mount);
		char member_uuid_str[40];
		char data_allowed_str[100];
		char data_has_str[100];
		char group[BCH_SB_LABEL_SIZE+10];
		char time_str[64];

		if (!bch2_member_exists(m))
			continue;

		uuid_unparse(m->uuid.b, member_uuid_str);

		if (BCH_MEMBER_GROUP(m)) {
			unsigned idx = BCH_MEMBER_GROUP(m) - 1;

			if (idx < disk_groups_nr(gi)) {
				snprintf(group, sizeof(group), "%.*s (%u)",
					BCH_SB_LABEL_SIZE,
					gi->entries[idx].label, idx);
			} else {
				strcpy(group, "(bad disk groups section)");
			}
		} else {
			strcpy(group, "(none)");
		}

		bch2_flags_to_text(&PBUF(data_allowed_str),
				   bch2_data_types,
				   BCH_MEMBER_DATA_ALLOWED(m));
		if (!data_allowed_str[0])
			strcpy(data_allowed_str, "(none)");

		bch2_flags_to_text(&PBUF(data_has_str),
				   bch2_data_types,
				   get_dev_has_data(sb, i));
		if (!data_has_str[0])
			strcpy(data_has_str, "(none)");

		if (last_mount) {
			struct tm *tm = localtime(&last_mount);
			size_t err = strftime(time_str, sizeof(time_str), "%c", tm);
			if (!err)
				strcpy(time_str, "(formatting error)");
		} else {
			strcpy(time_str, "(never)");
		}

		printf("  Device %u:\n"
		       "    UUID:			%s\n"
		       "    Size:			%s\n"
		       "    Bucket size:		%s\n"
		       "    First bucket:		%u\n"
		       "    Buckets:			%llu\n"
		       "    Last mount:			%s\n"
		       "    State:			%s\n"
		       "    Group:			%s\n"
		       "    Data allowed:		%s\n"

		       "    Has data:			%s\n"

		       "    Replacement policy:		%s\n"
		       "    Discard:			%llu\n",
		       i, member_uuid_str,
		       pr_units(le16_to_cpu(m->bucket_size) *
				le64_to_cpu(m->nbuckets), units),
		       pr_units(le16_to_cpu(m->bucket_size), units),
		       le16_to_cpu(m->first_bucket),
		       le64_to_cpu(m->nbuckets),
		       time_str,

		       BCH_MEMBER_STATE(m) < BCH_MEMBER_STATE_NR
		       ? bch2_dev_state[BCH_MEMBER_STATE(m)]
		       : "unknown",

		       group,
		       data_allowed_str,
		       data_has_str,

		       BCH_MEMBER_REPLACEMENT(m) < CACHE_REPLACEMENT_NR
		       ? bch2_cache_replacement_policies[BCH_MEMBER_REPLACEMENT(m)]
		       : "unknown",

		       BCH_MEMBER_DISCARD(m));
	}
}

static void bch2_sb_print_crypt(struct bch_sb *sb, struct bch_sb_field *f,
				enum units units)
{
	struct bch_sb_field_crypt *crypt = field_to_type(f, crypt);

	printf("  KFD:			%llu\n"
	       "  scrypt n:		%llu\n"
	       "  scrypt r:		%llu\n"
	       "  scrypt p:		%llu\n",
	       BCH_CRYPT_KDF_TYPE(crypt),
	       BCH_KDF_SCRYPT_N(crypt),
	       BCH_KDF_SCRYPT_R(crypt),
	       BCH_KDF_SCRYPT_P(crypt));
}

static void bch2_sb_print_replicas_v0(struct bch_sb *sb, struct bch_sb_field *f,
				   enum units units)
{
	struct bch_sb_field_replicas_v0 *replicas = field_to_type(f, replicas_v0);
	struct bch_replicas_entry_v0 *e;
	unsigned i;

	for_each_replicas_entry(replicas, e) {
		printf_pad(32, "  %s:", bch2_data_types[e->data_type]);

		putchar('[');
		for (i = 0; i < e->nr_devs; i++) {
			if (i)
				putchar(' ');
			printf("%u", e->devs[i]);
		}
		printf("]\n");
	}
}

static void bch2_sb_print_replicas(struct bch_sb *sb, struct bch_sb_field *f,
				   enum units units)
{
	struct bch_sb_field_replicas *replicas = field_to_type(f, replicas);
	struct bch_replicas_entry *e;
	unsigned i;

	for_each_replicas_entry(replicas, e) {
		printf_pad(32, "  %s: %u/%u",
			   bch2_data_types[e->data_type],
			   e->nr_required,
			   e->nr_devs);

		putchar('[');
		for (i = 0; i < e->nr_devs; i++) {
			if (i)
				putchar(' ');
			printf("%u", e->devs[i]);
		}
		printf("]\n");
	}
}

static void bch2_sb_print_quota(struct bch_sb *sb, struct bch_sb_field *f,
				enum units units)
{
}

static void bch2_sb_print_disk_groups(struct bch_sb *sb, struct bch_sb_field *f,
				      enum units units)
{
}

static void bch2_sb_print_clean(struct bch_sb *sb, struct bch_sb_field *f,
				enum units units)
{
	struct bch_sb_field_clean *clean = field_to_type(f, clean);


	printf("  flags:       %x", le32_to_cpu(clean->flags));
	printf("  journal seq: %llx", le64_to_cpu(clean->journal_seq));
}

static void bch2_sb_print_journal_seq_blacklist(struct bch_sb *sb, struct bch_sb_field *f,
				enum units units)
{
	struct bch_sb_field_journal_seq_blacklist *bl = field_to_type(f, journal_seq_blacklist);
	unsigned i, nr = blacklist_nr_entries(bl);

	for (i = 0; i < nr; i++) {
		struct journal_seq_blacklist_entry *e =
			bl->start + i;

		printf("  %llu-%llu\n",
		       le64_to_cpu(e->start),
		       le64_to_cpu(e->end));
	}
}

typedef void (*sb_field_print_fn)(struct bch_sb *, struct bch_sb_field *, enum units);

struct bch_sb_field_toolops {
	sb_field_print_fn	print;
};

static const struct bch_sb_field_toolops bch2_sb_field_ops[] = {
#define x(f, nr)					\
	[BCH_SB_FIELD_##f] = {				\
		.print	= bch2_sb_print_##f,		\
	},
	BCH_SB_FIELDS()
#undef x
};

static inline void bch2_sb_field_print(struct bch_sb *sb,
				       struct bch_sb_field *f,
				       enum units units)
{
	unsigned type = le32_to_cpu(f->type);

	if (type < BCH_SB_FIELD_NR)
		bch2_sb_field_ops[type].print(sb, f, units);
	else
		printf("(unknown field %u)\n", type);
}

void bch2_sb_print(struct bch_sb *sb, bool print_layout,
		   unsigned fields, enum units units)
{
	struct bch_sb_field_members *mi;
	char user_uuid_str[40], internal_uuid_str[40];
	char features_str[500];
	char fields_have_str[200];
	char label[BCH_SB_LABEL_SIZE + 1];
	char time_str[64];
	char foreground_str[64];
	char background_str[64];
	char promote_str[64];
	char metadata_str[64];
	struct bch_sb_field *f;
	u64 fields_have = 0;
	unsigned nr_devices = 0;
	time_t time_base = le64_to_cpu(sb->time_base_lo) / NSEC_PER_SEC;

	memcpy(label, sb->label, BCH_SB_LABEL_SIZE);
	label[BCH_SB_LABEL_SIZE] = '\0';

	uuid_unparse(sb->user_uuid.b, user_uuid_str);
	uuid_unparse(sb->uuid.b, internal_uuid_str);

	if (time_base) {
		struct tm *tm = localtime(&time_base);
		size_t err = strftime(time_str, sizeof(time_str), "%c", tm);
		if (!err)
			strcpy(time_str, "(formatting error)");
	} else {
		strcpy(time_str, "(not set)");
	}

	mi = bch2_sb_get_members(sb);
	if (mi) {
		struct bch_member *m;

		for (m = mi->members;
		     m < mi->members + sb->nr_devices;
		     m++)
			nr_devices += bch2_member_exists(m);
	}

	bch2_sb_get_target(sb, foreground_str, sizeof(foreground_str),
		BCH_SB_FOREGROUND_TARGET(sb));

	bch2_sb_get_target(sb, background_str, sizeof(background_str),
		BCH_SB_BACKGROUND_TARGET(sb));

	bch2_sb_get_target(sb, promote_str, sizeof(promote_str),
		BCH_SB_PROMOTE_TARGET(sb));

	bch2_sb_get_target(sb, metadata_str, sizeof(metadata_str),
		BCH_SB_METADATA_TARGET(sb));

	bch2_flags_to_text(&PBUF(features_str),
			   bch2_sb_features,
			   le64_to_cpu(sb->features[0]));

	vstruct_for_each(sb, f)
		fields_have |= 1 << le32_to_cpu(f->type);
	bch2_flags_to_text(&PBUF(fields_have_str),
			   bch2_sb_fields, fields_have);

	printf("External UUID:			%s\n"
	       "Internal UUID:			%s\n"
	       "Device index:			%u\n"
	       "Label:				%s\n"
	       "Version:			%llu\n"
	       "Created:			%s\n"
	       "Squence number:			%llu\n"
	       "Block_size:			%s\n"
	       "Btree node size:		%s\n"
	       "Error action:			%s\n"
	       "Clean:				%llu\n"
	       "Features:			%s\n"

	       "Metadata replicas:		%llu\n"
	       "Data replicas:			%llu\n"

	       "Metadata checksum type:		%s (%llu)\n"
	       "Data checksum type:		%s (%llu)\n"
	       "Compression type:		%s (%llu)\n"

	       "Foreground write target:	%s\n"
	       "Background write target:	%s\n"
	       "Promote target:			%s\n"
               "Metadata target:                %s\n"

	       "String hash type:		%s (%llu)\n"
	       "32 bit inodes:			%llu\n"
	       "GC reserve percentage:		%llu%%\n"
	       "Root reserve percentage:	%llu%%\n"

	       "Devices:			%u live, %u total\n"
	       "Sections:			%s\n"
	       "Superblock size:		%llu\n",
	       user_uuid_str,
	       internal_uuid_str,
	       sb->dev_idx,
	       label,
	       le64_to_cpu(sb->version),
	       time_str,
	       le64_to_cpu(sb->seq),
	       pr_units(le16_to_cpu(sb->block_size), units),
	       pr_units(BCH_SB_BTREE_NODE_SIZE(sb), units),

	       BCH_SB_ERROR_ACTION(sb) < BCH_NR_ERROR_ACTIONS
	       ? bch2_error_actions[BCH_SB_ERROR_ACTION(sb)]
	       : "unknown",

	       BCH_SB_CLEAN(sb),
	       features_str,

	       BCH_SB_META_REPLICAS_WANT(sb),
	       BCH_SB_DATA_REPLICAS_WANT(sb),

	       BCH_SB_META_CSUM_TYPE(sb) < BCH_CSUM_OPT_NR
	       ? bch2_csum_opts[BCH_SB_META_CSUM_TYPE(sb)]
	       : "unknown",
	       BCH_SB_META_CSUM_TYPE(sb),

	       BCH_SB_DATA_CSUM_TYPE(sb) < BCH_CSUM_OPT_NR
	       ? bch2_csum_opts[BCH_SB_DATA_CSUM_TYPE(sb)]
	       : "unknown",
	       BCH_SB_DATA_CSUM_TYPE(sb),

	       BCH_SB_COMPRESSION_TYPE(sb) < BCH_COMPRESSION_OPT_NR
	       ? bch2_compression_opts[BCH_SB_COMPRESSION_TYPE(sb)]
	       : "unknown",
	       BCH_SB_COMPRESSION_TYPE(sb),

	       foreground_str,
	       background_str,
	       promote_str,
               metadata_str,

	       BCH_SB_STR_HASH_TYPE(sb) < BCH_STR_HASH_NR
	       ? bch2_str_hash_types[BCH_SB_STR_HASH_TYPE(sb)]
	       : "unknown",
	       BCH_SB_STR_HASH_TYPE(sb),

	       BCH_SB_INODE_32BIT(sb),
	       BCH_SB_GC_RESERVE(sb),
	       BCH_SB_ROOT_RESERVE(sb),

	       nr_devices, sb->nr_devices,
	       fields_have_str,
	       vstruct_bytes(sb));

	if (print_layout) {
		printf("\n"
		       "Layout:\n");
		bch2_sb_print_layout(sb, units);
	}

	vstruct_for_each(sb, f) {
		unsigned type = le32_to_cpu(f->type);
		char name[60];

		if (!(fields & (1 << type)))
			continue;

		if (type < BCH_SB_FIELD_NR) {
			scnprintf(name, sizeof(name), "%s", bch2_sb_fields[type]);
			name[0] = toupper(name[0]);
		} else {
			scnprintf(name, sizeof(name), "(unknown field %u)", type);
		}

		printf("\n%s (size %llu):\n", name, vstruct_bytes(f));
		if (type < BCH_SB_FIELD_NR)
			bch2_sb_field_print(sb, f, units);
	}
}

/* ioctl interface: */

/* Global control device: */
int bcachectl_open(void)
{
	return xopen("/dev/bcachefs-ctl", O_RDWR);
}

/* Filesystem handles (ioctl, sysfs dir): */

#define SYSFS_BASE "/sys/fs/bcachefs/"

void bcache_fs_close(struct bchfs_handle fs)
{
	close(fs.ioctl_fd);
	close(fs.sysfs_fd);
}

struct bchfs_handle bcache_fs_open(const char *path)
{
	struct bchfs_handle ret;

	if (!uuid_parse(path, ret.uuid.b)) {
		/* It's a UUID, look it up in sysfs: */
		char *sysfs = mprintf(SYSFS_BASE "%s", path);
		ret.sysfs_fd = xopen(sysfs, O_RDONLY);

		char *minor = read_file_str(ret.sysfs_fd, "minor");
		char *ctl = mprintf("/dev/bcachefs%s-ctl", minor);
		ret.ioctl_fd = xopen(ctl, O_RDWR);

		free(sysfs);
		free(minor);
		free(ctl);
	} else {
		/* It's a path: */
		ret.ioctl_fd = xopen(path, O_RDONLY);

		struct bch_ioctl_query_uuid uuid;
		if (ioctl(ret.ioctl_fd, BCH_IOCTL_QUERY_UUID, &uuid) < 0)
			die("error opening %s: not a bcachefs filesystem", path);

		ret.uuid = uuid.uuid;

		char uuid_str[40];
		uuid_unparse(uuid.uuid.b, uuid_str);

		char *sysfs = mprintf(SYSFS_BASE "%s", uuid_str);
		ret.sysfs_fd = xopen(sysfs, O_RDONLY);
		free(sysfs);
	}

	return ret;
}

/*
 * Given a path to a block device, open the filesystem it belongs to; also
 * return the device's idx:
 */
struct bchfs_handle bchu_fs_open_by_dev(const char *path, unsigned *idx)
{
	char buf[1024], *uuid_str;

	struct stat stat = xstat(path);

	if (!S_ISBLK(stat.st_mode))
		die("%s is not a block device", path);

	char *sysfs = mprintf("/sys/dev/block/%u:%u/bcachefs",
			      major(stat.st_dev),
			      minor(stat.st_dev));
	ssize_t len = readlink(sysfs, buf, sizeof(buf));
	free(sysfs);

	if (len > 0) {
		char *p = strrchr(buf, '/');
		if (!p || sscanf(p + 1, "dev-%u", idx) != 1)
			die("error parsing sysfs");

		*p = '\0';
		p = strrchr(buf, '/');
		uuid_str = p + 1;
	} else {
		struct bch_opts opts = bch2_opts_empty();

		opt_set(opts, noexcl,	true);
		opt_set(opts, nochanges, true);

		struct bch_sb_handle sb;
		int ret = bch2_read_super(path, &opts, &sb);
		if (ret)
			die("Error opening %s: %s", path, strerror(-ret));

		*idx = sb.sb->dev_idx;
		uuid_str = buf;
		uuid_unparse(sb.sb->user_uuid.b, uuid_str);

		bch2_free_super(&sb);
	}

	return bcache_fs_open(uuid_str);
}

int bchu_data(struct bchfs_handle fs, struct bch_ioctl_data cmd)
{
	int progress_fd = xioctl(fs.ioctl_fd, BCH_IOCTL_DATA, &cmd);

	while (1) {
		struct bch_ioctl_data_event e;

		if (read(progress_fd, &e, sizeof(e)) != sizeof(e))
			die("error reading from progress fd %m");

		if (e.type)
			continue;

		if (e.p.data_type == U8_MAX)
			break;

		printf("\33[2K\r");

		printf("%llu%% complete: current position %s",
		       e.p.sectors_total
		       ? e.p.sectors_done * 100 / e.p.sectors_total
		       : 0,
		       bch2_data_types[e.p.data_type]);

		switch (e.p.data_type) {
		case BCH_DATA_btree:
		case BCH_DATA_user:
			printf(" %s:%llu:%llu",
			       bch2_btree_ids[e.p.btree_id],
			       e.p.pos.inode,
			       e.p.pos.offset);
		}

		fflush(stdout);
		sleep(1);
	}
	printf("\nDone\n");

	close(progress_fd);
	return 0;
}

/* option parsing */

struct bch_opt_strs bch2_cmdline_opts_get(int *argc, char *argv[],
					  unsigned opt_types)
{
	struct bch_opt_strs opts;
	unsigned i = 1;

	memset(&opts, 0, sizeof(opts));

	while (i < *argc) {
		char *optstr = strcmp_prefix(argv[i], "--");
		char *valstr = NULL, *p;
		int optid, nr_args = 1;

		if (!optstr) {
			i++;
			continue;
		}

		optstr = strdup(optstr);

		p = optstr;
		while (isalpha(*p) || *p == '_')
			p++;

		if (*p == '=') {
			*p = '\0';
			valstr = p + 1;
		}

		optid = bch2_opt_lookup(optstr);
		if (optid < 0 ||
		    !(bch2_opt_table[optid].mode & opt_types)) {
			free(optstr);
			i++;
			continue;
		}

		if (!valstr &&
		    bch2_opt_table[optid].type != BCH_OPT_BOOL) {
			nr_args = 2;
			valstr = argv[i + 1];
		}

		if (!valstr)
			valstr = "1";

		opts.by_id[optid] = valstr;

		*argc -= nr_args;
		memmove(&argv[i],
			&argv[i + nr_args],
			sizeof(char *) * (*argc - i));
		argv[*argc] = NULL;
	}

	return opts;
}

struct bch_opts bch2_parse_opts(struct bch_opt_strs strs)
{
	struct bch_opts opts = bch2_opts_empty();
	unsigned i;
	int ret;
	u64 v;

	for (i = 0; i < bch2_opts_nr; i++) {
		if (!strs.by_id[i] ||
		    bch2_opt_table[i].type == BCH_OPT_FN)
			continue;

		ret = bch2_opt_parse(NULL, &bch2_opt_table[i],
				     strs.by_id[i], &v);
		if (ret < 0)
			die("Invalid %s: %s",
			    bch2_opt_table[i].attr.name,
			    strerror(-ret));

		bch2_opt_set_by_id(&opts, i, v);
	}

	return opts;
}

void bch2_opts_usage(unsigned opt_types)
{
	const struct bch_option *opt;
	unsigned i, c = 0, helpcol = 30;

	void tabalign() {
		while (c < helpcol) {
			putchar(' ');
			c++;
		}
	}

	void newline() {
		printf("\n");
		c = 0;
	}

	for (opt = bch2_opt_table;
	     opt < bch2_opt_table + bch2_opts_nr;
	     opt++) {
		if (!(opt->mode & opt_types))
			continue;

		c += printf("      --%s", opt->attr.name);

		switch (opt->type) {
		case BCH_OPT_BOOL:
			break;
		case BCH_OPT_STR:
			c += printf("=(");
			for (i = 0; opt->choices[i]; i++) {
				if (i)
					c += printf("|");
				c += printf("%s", opt->choices[i]);
			}
			c += printf(")");
			break;
		default:
			c += printf("=%s", opt->hint);
			break;
		}

		if (opt->help) {
			const char *l = opt->help;

			if (c >= helpcol)
				newline();

			while (1) {
				const char *n = strchrnul(l, '\n');

				tabalign();
				printf("%.*s", (int) (n - l), l);
				newline();

				if (!*n)
					break;
				l = n + 1;
			}
		} else {
			newline();
		}
	}
}

dev_names bchu_fs_get_devices(struct bchfs_handle fs)
{
	DIR *dir = fdopendir(fs.sysfs_fd);
	struct dirent *d;
	dev_names devs;

	darray_init(devs);

	while ((errno = 0), (d = readdir(dir))) {
		struct dev_name n = { 0, NULL, NULL };

		if (sscanf(d->d_name, "dev-%u", &n.idx) != 1)
			continue;

		char *block_attr = mprintf("dev-%u/block", n.idx);

		char sysfs_block_buf[4096];
		ssize_t r = readlinkat(fs.sysfs_fd, block_attr,
				       sysfs_block_buf, sizeof(sysfs_block_buf));
		if (r > 0) {
			sysfs_block_buf[r] = '\0';
			n.dev = strdup(basename(sysfs_block_buf));
		}

		free(block_attr);

		char *label_attr = mprintf("dev-%u/label", n.idx);
		n.label = read_file_str(fs.sysfs_fd, label_attr);
		free(label_attr);

		darray_append(devs, n);
	}

	closedir(dir);

	return devs;
}
