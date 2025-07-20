#include <assert.h>
#include <ctype.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <linux/fs.h>
#include <math.h>
#include <stdbool.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/types.h>
#include <unistd.h>

#include <blkid.h>
#include <uuid/uuid.h>

#include "libbcachefs.h"
#include "libbcachefs/bcachefs_ioctl.h"
#include "linux/sort.h"
#include "tools-util.h"
#include "libbcachefs/util.h"

void die(const char *fmt, ...)
{
	va_list args;

	va_start(args, fmt);
	vfprintf(stderr, fmt, args);
	va_end(args);
	fputc('\n', stderr);

	_exit(EXIT_FAILURE);
}

char *mprintf(const char *fmt, ...)
{
	va_list args;
	char *str;
	int ret;

	va_start(args, fmt);
	ret = vasprintf(&str, fmt, args);
	va_end(args);

	if (ret < 0)
		die("insufficient memory");

	return str;
}

void xpread(int fd, void *buf, size_t count, off_t offset)
{
	while (count) {
		ssize_t r = pread(fd, buf, count, offset);

		if (r < 0)
			die("read error: %m");
		if (!r)
			die("pread error: unexpected eof");
		count	-= r;
		offset	+= r;
	}
}

void xpwrite(int fd, const void *buf, size_t count, off_t offset, const char *msg)
{
	ssize_t r = pwrite(fd, buf, count, offset);

	if (r != count)
		die("error writing %s (ret %zi err %m)", msg, r);
}

struct stat xfstatat(int dirfd, const char *path, int flags)
{
	struct stat stat;
	if (fstatat(dirfd, path, &stat, flags))
		die("stat error: %m");
	return stat;
}

struct stat xfstat(int fd)
{
	struct stat stat;
	if (fstat(fd, &stat))
		die("stat error: %m");
	return stat;
}

struct stat xstat(const char *path)
{
	struct stat statbuf;
	if (stat(path, &statbuf))
		die("stat error statting %s: %m", path);
	return statbuf;
}

/* File parsing (i.e. sysfs) */

void write_file_str(int dirfd, const char *path, const char *str)
{
	int fd = xopenat(dirfd, path, O_WRONLY);
	ssize_t wrote, len = strlen(str);

	wrote = write(fd, str, len);
	if (wrote != len)
		die("read error: %m");
	xclose(fd);
}

char *read_file_str(int dirfd, const char *path)
{
	int fd = xopenat(dirfd, path, O_RDONLY);
	ssize_t len = xfstat(fd).st_size;

	char *buf = xmalloc(len + 1);

	len = read(fd, buf, len);
	if (len < 0)
		die("read error: %m");

	buf[len] = '\0';
	if (len && buf[len - 1] == '\n')
		buf[len - 1] = '\0';
	if (!strlen(buf)) {
		free(buf);
		buf = NULL;
	}

	xclose(fd);

	return buf;
}

u64 read_file_u64(int dirfd, const char *path)
{
	char *buf = read_file_str(dirfd, path);
	u64 v;
	if (bch2_strtou64_h(buf, &v))
		die("read_file_u64: error parsing %s (got %s)", path, buf);
	free(buf);
	return v;
}

/* String list options: */

ssize_t read_string_list_or_die(const char *opt, const char * const list[],
				const char *msg)
{
	ssize_t v = match_string(list, -1, opt);
	if (v < 0)
		die("Bad %s %s", msg, opt);

	return v;
}

u64 read_flag_list_or_die(char *opt, const char * const list[],
			  const char *msg)
{
	u64 v = bch2_read_flag_list(opt, list);
	if (v == (u64) -1)
		die("Bad %s %s", msg, opt);

	return v;
}

/* Returns size of file or block device: */
u64 get_size(int fd)
{
	struct stat statbuf = xfstat(fd);

	if (!S_ISBLK(statbuf.st_mode))
		return statbuf.st_size;

	u64 ret;
	xioctl(fd, BLKGETSIZE64, &ret);
	return ret;
}

/* Returns blocksize, in bytes: */
unsigned get_blocksize(int fd)
{
	struct stat statbuf = xfstat(fd);

	if (!S_ISBLK(statbuf.st_mode))
		return statbuf.st_blksize;

	unsigned ret;
	xioctl(fd, BLKPBSZGET, &ret);
	return ret;
}

/* Open a block device, do magic blkid stuff to probe for existing filesystems: */
int open_for_format(struct dev_opts *dev, blk_mode_t mode, bool force)
{
	int blkid_version_code = blkid_get_library_version(NULL, NULL);
	if (blkid_version_code < 2401) {
		if (force) {
			fprintf(
				stderr,
				"Continuing with out of date libblkid %s because --force was passed.\n",
				BLKID_VERSION);
		} else {
			// Reference for picking 2.40.1:
			// https://mirrors.edge.kernel.org/pub/linux/utils/util-linux/v2.40/v2.40.1-ReleaseNotes
			// https://github.com/util-linux/util-linux/issues/3103
			die(
				"Refusing to format when using libblkid %s\n"
				"libblkid >= 2.40.1 is required to check for existing filesystems\n"
				"Earlier versions may not recognize some bcachefs filesystems.\n", BLKID_VERSION);
		}
	}

	blkid_probe pr;
	const char *fs_type = NULL, *fs_label = NULL;
	size_t fs_type_len, fs_label_len;

	dev->file = bdev_file_open_by_path(dev->path,
				BLK_OPEN_READ|BLK_OPEN_WRITE|BLK_OPEN_EXCL|BLK_OPEN_BUFFERED|mode,
				dev, NULL);
	int ret = PTR_ERR_OR_ZERO(dev->file);
	if (ret < 0)
		die("Error opening device to format %s: %s", dev->path, strerror(-ret));
	dev->bdev = file_bdev(dev->file);

	if (!(pr = blkid_new_probe()))
		die("blkid error 1");
	if (blkid_probe_set_device(pr, dev->bdev->bd_fd, 0, 0))
		die("blkid error 2");
	if (blkid_probe_enable_partitions(pr, true) ||
	    blkid_probe_enable_superblocks(pr, true) ||
	    blkid_probe_set_superblocks_flags(pr,
			BLKID_SUBLKS_LABEL|BLKID_SUBLKS_TYPE|BLKID_SUBLKS_MAGIC))
		die("blkid error 3");
	if (blkid_do_fullprobe(pr) < 0)
		die("blkid error 4");

	blkid_probe_lookup_value(pr, "TYPE", &fs_type, &fs_type_len);
	blkid_probe_lookup_value(pr, "LABEL", &fs_label, &fs_label_len);

	if (fs_type) {
		if (fs_label)
			printf("%s contains a %s filesystem labelled '%s'\n",
			       dev->path, fs_type, fs_label);
		else
			printf("%s contains a %s filesystem\n",
			       dev->path, fs_type);
		if (!force) {
			fputs("Proceed anyway?", stdout);
			if (!ask_yn())
				exit(EXIT_FAILURE);
		}
		while (blkid_do_probe(pr) == 0) {
			if (blkid_do_wipe(pr, 0))
				die("Failed to wipe preexisting metadata.");
		}
	}

	blkid_free_probe(pr);
	return ret;
}

bool ask_yn(void)
{
	const char *short_yes = "yY";
	char *buf = NULL;
	size_t buflen = 0;
	bool ret;

	fputs(" (y,n) ", stdout);
	fflush(stdout);

	if (getline(&buf, &buflen, stdin) < 0)
		die("error reading from standard input");

	ret = strchr(short_yes, buf[0]);
	free(buf);
	return ret;
}

static int range_cmp(const void *_l, const void *_r)
{
	const struct range *l = _l, *r = _r;

	if (l->start < r->start)
		return -1;
	if (l->start > r->start)
		return  1;
	return 0;
}

void ranges_sort(ranges *r)
{
	sort(r->data, r->nr, sizeof(r->data[0]), range_cmp, NULL);
}

void ranges_sort_merge(ranges *r)
{
	ranges tmp = { 0 };

	ranges_sort(r);

	/* Merge contiguous ranges: */
	darray_for_each(*r, i) {
		struct range *t = tmp.nr ? &tmp.data[tmp.nr - 1] : NULL;

		if (t && t->end >= i->start)
			t->end = max(t->end, i->end);
		else
			darray_push(&tmp, *i);
	}

	darray_exit(r);
	*r = tmp;
}

void ranges_roundup(ranges *r, unsigned block_size)
{
	darray_for_each(*r, i) {
		i->start = round_down(i->start, block_size);
		i->end	= round_up(i->end, block_size);
	}
}

void ranges_rounddown(ranges *r, unsigned block_size)
{
	darray_for_each(*r, i) {
		i->start = round_up(i->start, block_size);
		i->end	= round_down(i->end, block_size);
		i->end	= max(i->end, i->start);
	}
}

struct fiemap_extent fiemap_iter_next(struct fiemap_iter *iter)
{
	struct fiemap_extent e;

	BUG_ON(iter->idx > iter->f->fm_mapped_extents);

	if (iter->idx == iter->f->fm_mapped_extents) {
		xioctl(iter->fd, FS_IOC_FIEMAP, iter->f);

		if (!iter->f->fm_mapped_extents)
			return (struct fiemap_extent) { .fe_length = 0 };

		iter->idx = 0;
	}

	e = iter->f->fm_extents[iter->idx++];
	BUG_ON(!e.fe_length);

	iter->f->fm_start = e.fe_logical + e.fe_length;

	return e;
}

char *strcmp_prefix(char *a, const char *a_prefix)
{
	while (*a_prefix && *a == *a_prefix) {
		a++;
		a_prefix++;
	}
	return *a_prefix ? NULL : a;
}

/* crc32c */

static u32 crc32c_default(u32 crc, const void *buf, size_t size)
{
	static const u32 crc32c_tab[] = {
		0x00000000, 0xF26B8303, 0xE13B70F7, 0x1350F3F4,
		0xC79A971F, 0x35F1141C, 0x26A1E7E8, 0xD4CA64EB,
		0x8AD958CF, 0x78B2DBCC, 0x6BE22838, 0x9989AB3B,
		0x4D43CFD0, 0xBF284CD3, 0xAC78BF27, 0x5E133C24,
		0x105EC76F, 0xE235446C, 0xF165B798, 0x030E349B,
		0xD7C45070, 0x25AFD373, 0x36FF2087, 0xC494A384,
		0x9A879FA0, 0x68EC1CA3, 0x7BBCEF57, 0x89D76C54,
		0x5D1D08BF, 0xAF768BBC, 0xBC267848, 0x4E4DFB4B,
		0x20BD8EDE, 0xD2D60DDD, 0xC186FE29, 0x33ED7D2A,
		0xE72719C1, 0x154C9AC2, 0x061C6936, 0xF477EA35,
		0xAA64D611, 0x580F5512, 0x4B5FA6E6, 0xB93425E5,
		0x6DFE410E, 0x9F95C20D, 0x8CC531F9, 0x7EAEB2FA,
		0x30E349B1, 0xC288CAB2, 0xD1D83946, 0x23B3BA45,
		0xF779DEAE, 0x05125DAD, 0x1642AE59, 0xE4292D5A,
		0xBA3A117E, 0x4851927D, 0x5B016189, 0xA96AE28A,
		0x7DA08661, 0x8FCB0562, 0x9C9BF696, 0x6EF07595,
		0x417B1DBC, 0xB3109EBF, 0xA0406D4B, 0x522BEE48,
		0x86E18AA3, 0x748A09A0, 0x67DAFA54, 0x95B17957,
		0xCBA24573, 0x39C9C670, 0x2A993584, 0xD8F2B687,
		0x0C38D26C, 0xFE53516F, 0xED03A29B, 0x1F682198,
		0x5125DAD3, 0xA34E59D0, 0xB01EAA24, 0x42752927,
		0x96BF4DCC, 0x64D4CECF, 0x77843D3B, 0x85EFBE38,
		0xDBFC821C, 0x2997011F, 0x3AC7F2EB, 0xC8AC71E8,
		0x1C661503, 0xEE0D9600, 0xFD5D65F4, 0x0F36E6F7,
		0x61C69362, 0x93AD1061, 0x80FDE395, 0x72966096,
		0xA65C047D, 0x5437877E, 0x4767748A, 0xB50CF789,
		0xEB1FCBAD, 0x197448AE, 0x0A24BB5A, 0xF84F3859,
		0x2C855CB2, 0xDEEEDFB1, 0xCDBE2C45, 0x3FD5AF46,
		0x7198540D, 0x83F3D70E, 0x90A324FA, 0x62C8A7F9,
		0xB602C312, 0x44694011, 0x5739B3E5, 0xA55230E6,
		0xFB410CC2, 0x092A8FC1, 0x1A7A7C35, 0xE811FF36,
		0x3CDB9BDD, 0xCEB018DE, 0xDDE0EB2A, 0x2F8B6829,
		0x82F63B78, 0x709DB87B, 0x63CD4B8F, 0x91A6C88C,
		0x456CAC67, 0xB7072F64, 0xA457DC90, 0x563C5F93,
		0x082F63B7, 0xFA44E0B4, 0xE9141340, 0x1B7F9043,
		0xCFB5F4A8, 0x3DDE77AB, 0x2E8E845F, 0xDCE5075C,
		0x92A8FC17, 0x60C37F14, 0x73938CE0, 0x81F80FE3,
		0x55326B08, 0xA759E80B, 0xB4091BFF, 0x466298FC,
		0x1871A4D8, 0xEA1A27DB, 0xF94AD42F, 0x0B21572C,
		0xDFEB33C7, 0x2D80B0C4, 0x3ED04330, 0xCCBBC033,
		0xA24BB5A6, 0x502036A5, 0x4370C551, 0xB11B4652,
		0x65D122B9, 0x97BAA1BA, 0x84EA524E, 0x7681D14D,
		0x2892ED69, 0xDAF96E6A, 0xC9A99D9E, 0x3BC21E9D,
		0xEF087A76, 0x1D63F975, 0x0E330A81, 0xFC588982,
		0xB21572C9, 0x407EF1CA, 0x532E023E, 0xA145813D,
		0x758FE5D6, 0x87E466D5, 0x94B49521, 0x66DF1622,
		0x38CC2A06, 0xCAA7A905, 0xD9F75AF1, 0x2B9CD9F2,
		0xFF56BD19, 0x0D3D3E1A, 0x1E6DCDEE, 0xEC064EED,
		0xC38D26C4, 0x31E6A5C7, 0x22B65633, 0xD0DDD530,
		0x0417B1DB, 0xF67C32D8, 0xE52CC12C, 0x1747422F,
		0x49547E0B, 0xBB3FFD08, 0xA86F0EFC, 0x5A048DFF,
		0x8ECEE914, 0x7CA56A17, 0x6FF599E3, 0x9D9E1AE0,
		0xD3D3E1AB, 0x21B862A8, 0x32E8915C, 0xC083125F,
		0x144976B4, 0xE622F5B7, 0xF5720643, 0x07198540,
		0x590AB964, 0xAB613A67, 0xB831C993, 0x4A5A4A90,
		0x9E902E7B, 0x6CFBAD78, 0x7FAB5E8C, 0x8DC0DD8F,
		0xE330A81A, 0x115B2B19, 0x020BD8ED, 0xF0605BEE,
		0x24AA3F05, 0xD6C1BC06, 0xC5914FF2, 0x37FACCF1,
		0x69E9F0D5, 0x9B8273D6, 0x88D28022, 0x7AB90321,
		0xAE7367CA, 0x5C18E4C9, 0x4F48173D, 0xBD23943E,
		0xF36E6F75, 0x0105EC76, 0x12551F82, 0xE03E9C81,
		0x34F4F86A, 0xC69F7B69, 0xD5CF889D, 0x27A40B9E,
		0x79B737BA, 0x8BDCB4B9, 0x988C474D, 0x6AE7C44E,
		0xBE2DA0A5, 0x4C4623A6, 0x5F16D052, 0xAD7D5351
	};
	const u8 *p = buf;

	while (size--)
		crc = crc32c_tab[(crc ^ *p++) & 0xFFL] ^ (crc >> 8);

	return crc;
}

#include <linux/compiler.h>

#ifdef __x86_64__

#ifdef CONFIG_X86_64
#define REX_PRE "0x48, "
#else
#define REX_PRE
#endif

static u32 crc32c_sse42(u32 crc, const void *buf, size_t size)
{
	while (size >= sizeof(long)) {
		const unsigned long *d = buf;

		__asm__ __volatile__(
			".byte 0xf2, " REX_PRE "0xf, 0x38, 0xf1, 0xf1;"
			:"=S"(crc)
			:"0"(crc), "c"(*d)
		);
		buf	+= sizeof(long);
		size	-= sizeof(long);
	}

	while (size) {
		const u8 *d = buf;

		__asm__ __volatile__(
			".byte 0xf2, 0xf, 0x38, 0xf0, 0xf1"
			:"=S"(crc)
			:"0"(crc), "c"(*d)
		);
		buf	+= 1;
		size	-= 1;
	}

	return crc;
}

#endif

static void *resolve_crc32c(void)
{
#ifdef __x86_64__
	if (__builtin_cpu_supports("sse4.2"))
		return crc32c_sse42;
#endif
	return crc32c_default;
}

/*
 * ifunc is buggy and I don't know what breaks it (LTO?)
 */
#ifdef HAVE_WORKING_IFUNC

static void *ifunc_resolve_crc32c(void)
{
	__builtin_cpu_init();

	return resolve_crc32c
}

u32 crc32c(u32, const void *, size_t)
	__attribute__((ifunc("ifunc_resolve_crc32c")));

#else

u32 crc32c(u32 crc, const void *buf, size_t size)
{
	static u32 (*real_crc32c)(u32, const void *, size_t);

	if (unlikely(!real_crc32c))
		real_crc32c = resolve_crc32c();

	return real_crc32c(crc, buf, size);
}

#endif /* HAVE_WORKING_IFUNC */

char *dev_to_name(dev_t dev)
{
	char *line = NULL, *name = NULL;
	size_t n = 0;

	FILE *f = fopen("/proc/partitions", "r");
	if (!f)
		die("error opening /proc/partitions: %m");

	while (getline(&line, &n, f) != -1) {
		unsigned ma, mi;
		u64 sectors;

		name = realloc(name, n + 1);

		if (sscanf(line, " %u %u %llu %s", &ma, &mi, &sectors, name) == 4 &&
		    ma == major(dev) && mi == minor(dev))
			goto found;
	}

	free(name);
	name = NULL;
found:
	fclose(f);
	free(line);
	return name;
}

char *dev_to_path(dev_t dev)
{
	char *name = dev_to_name(dev);
	if (!name)
		return NULL;

	char *path = mprintf("/dev/%s", name);

	free(name);
	return path;
}

struct mntent *dev_to_mount(const char *dev)
{
	struct mntent *mnt, *ret = NULL;
	FILE *f = setmntent("/proc/mounts", "r");
	if (!f)
		die("error opening /proc/mounts: %m");

	struct stat d1 = xstat(dev);

	while ((mnt = getmntent(f))) {
		char *d, *p = mnt->mnt_fsname;

		while ((d = strsep(&p, ":"))) {
			struct stat d2;

			if (stat(d, &d2))
				continue;

			if (S_ISBLK(d1.st_mode) != S_ISBLK(d2.st_mode))
				continue;

			if (S_ISBLK(d1.st_mode)) {
				if (d1.st_rdev != d2.st_rdev)
					continue;
			} else {
				if (d1.st_dev != d2.st_dev ||
				    d1.st_ino != d2.st_ino)
					continue;
			}

			ret = mnt;
			goto found;
		}
	}
found:
	fclose(f);
	return ret;
}

int dev_mounted(const char *dev)
{
	struct mntent *mnt = dev_to_mount(dev);

	if (!mnt)
		return 0;
	if (hasmntopt(mnt, "ro"))
		return 1;
	return 2;
}

static char *dev_to_sysfs_path(dev_t dev)
{
	return mprintf("/sys/dev/block/%u:%u", major(dev), minor(dev));
}

char *fd_to_dev_model(int fd)
{
	struct stat stat = xfstat(fd);

	if (S_ISBLK(stat.st_mode)) {
		char *sysfs_path = dev_to_sysfs_path(stat.st_rdev);

		char *model_path = mprintf("%s/device/model", sysfs_path);
		if (!access(model_path, R_OK))
			goto got_model;
		free(model_path);

		/* partition? try parent */

		char buf[1024];
		if (readlink(sysfs_path, buf, sizeof(buf)) < 0)
			die("readlink error on %s: %m", sysfs_path);

		free(sysfs_path);
		sysfs_path = strdup(buf);

		*strrchr(sysfs_path, '/') = 0;
		model_path = mprintf("%s/device/model", sysfs_path);
		if (!access(model_path, R_OK))
			goto got_model;

		return strdup("(unknown device)");
		char *model;
got_model:
		model = read_file_str(AT_FDCWD, model_path);
		free(model_path);
		free(sysfs_path);
		return model;
	} else {
		return strdup("(reg file)");
	}
}

static int kstrtoull_symbolic(const char *s, unsigned int base, unsigned long long *res)
{
	if (!strcmp(s, "U64_MAX")) {
		*res = U64_MAX;
		return 0;
	}

	if (!strcmp(s, "U32_MAX")) {
		*res = U32_MAX;
		return 0;
	}

	return kstrtoull(s, base, res);
}

static int kstrtouint_symbolic(const char *s, unsigned int base, unsigned *res)
{
	unsigned long long tmp;
	int rv;

	rv = kstrtoull_symbolic(s, base, &tmp);
	if (rv < 0)
		return rv;
	if (tmp != (unsigned long long)(unsigned int)tmp)
		return -ERANGE;
	*res = tmp;
	return 0;
}

struct bpos bpos_parse(char *buf)
{
	if (!strcmp(buf, "POS_MIN"))
		return POS_MIN;

	if (!strcmp(buf, "POS_MAX"))
		return POS_MAX;

	if (!strcmp(buf, "SPOS_MAX"))
		return SPOS_MAX;

	char *orig = strdup(buf);
	char *s = buf;

	char *inode_s	= strsep(&s, ":");
	char *offset_s	= strsep(&s, ":");
	char *snapshot_s = strsep(&s, ":");

	if (!inode_s || !offset_s || s)
		die("invalid bpos %s", orig);
	free(orig);

	u64 inode_v = 0, offset_v = 0;
	u32 snapshot_v = 0;
	if (kstrtoull_symbolic(inode_s, 10, &inode_v))
		die("invalid bpos.inode %s", inode_s);

	if (kstrtoull_symbolic(offset_s, 10, &offset_v))
		die("invalid bpos.offset %s", offset_s);

	if (snapshot_s &&
	    kstrtouint_symbolic(snapshot_s, 10, &snapshot_v))
		die("invalid bpos.snapshot %s", snapshot_s);

	return (struct bpos) { .inode = inode_v, .offset = offset_v, .snapshot = snapshot_v };
}

struct bbpos bbpos_parse(char *buf)
{
	char *s = buf, *field;
	struct bbpos ret;

	if (!(field = strsep(&s, ":")))
		die("invalid bbpos %s", buf);

	ret.btree = read_string_list_or_die(field, __bch2_btree_ids, "btree id");

	if (!s)
		die("invalid bbpos %s", buf);

	ret.pos = bpos_parse(s);
	return ret;
}

struct bbpos_range bbpos_range_parse(char *buf)
{
	char *s = buf;
	char *start_str = strsep(&s, "-");
	char *end_str	= strsep(&s, "-");

	struct bbpos start = bbpos_parse(start_str);
	struct bbpos end = end_str ? bbpos_parse(end_str) : start;

	return (struct bbpos_range) { .start = start, .end = end };
}

unsigned version_parse(char *buf)
{
	char *s = buf;
	char *major_str = strsep(&s, ".");
	char *minor_str	= strsep(&s, ".");

	unsigned major, minor;

	if (!minor_str) {
		major = 0;
		if (kstrtouint(major_str, 10, &minor))
			die("invalid version %s", buf);
	} else {

		if (kstrtouint(major_str, 10, &major) ||
		    kstrtouint(minor_str, 10, &minor))
			die("invalid version %s", buf);
	}

	return BCH_VERSION(major, minor);
}

darray_const_str get_or_split_cmdline_devs(int argc, char *argv[])
{
	darray_const_str ret = {};

	if (argc == 1) {
		bch2_split_devs(argv[0], &ret);
	} else {
		for (unsigned i = 0; i < argc; i++)
			darray_push(&ret, strdup(argv[i]));
	}

	return ret;
}

char *pop_cmd(int *argc, char *argv[])
{
	char *cmd = argv[1];
	if (!(*argc < 2))
		memmove(&argv[1], &argv[2], (*argc - 2) * sizeof(argv[0]));
	(*argc)--;
	argv[*argc] = NULL;

	return cmd;
}
