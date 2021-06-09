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

void *xcalloc(size_t count, size_t size)
{
	void *p = calloc(count, size);

	if (!p)
		die("insufficient memory");

	return p;
}

void *xmalloc(size_t size)
{
	void *p = malloc(size);

	if (!p)
		die("insufficient memory");

	memset(p, 0, size);
	return p;
}

void *xrealloc(void *p, size_t size)
{
	p = realloc(p, size);
	if (!p)
		die("insufficient memory");

	return p;
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

void xpwrite(int fd, const void *buf, size_t count, off_t offset)
{
	ssize_t r = pwrite(fd, buf, count, offset);

	if (r != count)
		die("write error (ret %zi err %m)", r);
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

/* Formatting: */

int printf_pad(unsigned pad, const char * fmt, ...)
{
       va_list args;
       int ret;

       va_start(args, fmt);
       ret = vprintf(fmt, args);
       va_end(args);

       while (ret++ < pad)
	       putchar(' ');

       return ret;
}

struct units_buf __pr_units(s64 _v, enum units units)
{
	struct units_buf ret;
	char *out = ret.b, *end = out + sizeof(ret.b);
	u64 v = _v;

	if (_v < 0) {
		out += scnprintf(out, end - out, "-");
		v = -_v;
	}

	switch (units) {
	case BYTES:
		snprintf(out, end - out, "%llu", v << 9);
		break;
	case SECTORS:
		snprintf(out, end - out, "%llu", v);
		break;
	case HUMAN_READABLE:
		v <<= 9;

		if (v >= 1024) {
			int exp = log(v) / log(1024);
			snprintf(out, end - out, "%.1f%c",
				 v / pow(1024, exp),
				 "KMGTPE"[exp-1]);
		} else {
			snprintf(out, end - out, "%llu", v);
		}

		break;
	}

	return ret;
}

/* Argument parsing stuff: */

/* File parsing (i.e. sysfs) */

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

	close(fd);

	return buf;
}

u64 read_file_u64(int dirfd, const char *path)
{
	char *buf = read_file_str(dirfd, path);
	u64 v;
	if (kstrtou64(buf, 10, &v))
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

/* Returns size of file or block device: */
u64 get_size(const char *path, int fd)
{
	struct stat statbuf = xfstat(fd);

	if (!S_ISBLK(statbuf.st_mode))
		return statbuf.st_size;

	u64 ret;
	xioctl(fd, BLKGETSIZE64, &ret);
	return ret;
}

/* Returns blocksize in units of 512 byte sectors: */
unsigned get_blocksize(const char *path, int fd)
{
	struct stat statbuf = xfstat(fd);

	if (!S_ISBLK(statbuf.st_mode))
		return statbuf.st_blksize >> 9;

	unsigned ret;
	xioctl(fd, BLKPBSZGET, &ret);
	return ret >> 9;
}

/* Open a block device, do magic blkid stuff to probe for existing filesystems: */
int open_for_format(const char *dev, bool force)
{
	blkid_probe pr;
	const char *fs_type = NULL, *fs_label = NULL;
	size_t fs_type_len, fs_label_len;

	int fd = xopen(dev, O_RDWR|O_EXCL);

	if (force)
		return fd;

	if (!(pr = blkid_new_probe()))
		die("blkid error 1");
	if (blkid_probe_set_device(pr, fd, 0, 0))
		die("blkid error 2");
	if (blkid_probe_enable_partitions(pr, true))
		die("blkid error 3");
	if (blkid_do_fullprobe(pr) < 0)
		die("blkid error 4");

	blkid_probe_lookup_value(pr, "TYPE", &fs_type, &fs_type_len);
	blkid_probe_lookup_value(pr, "LABEL", &fs_label, &fs_label_len);

	if (fs_type) {
		if (fs_label)
			printf("%s contains a %s filesystem labelled '%s'\n",
			       dev, fs_type, fs_label);
		else
			printf("%s contains a %s filesystem\n",
			       dev, fs_type);
		fputs("Proceed anyway?", stdout);
		if (!ask_yn())
			exit(EXIT_FAILURE);
		while (blkid_do_probe(pr) == 0)
    			blkid_do_wipe(pr, 0);
	}

	blkid_free_probe(pr);
	return fd;
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

void ranges_sort_merge(ranges *r)
{
	struct range *t, *i;
	ranges tmp = { NULL };

	sort(&darray_item(*r, 0), darray_size(*r),
	     sizeof(darray_item(*r, 0)), range_cmp, NULL);

	/* Merge contiguous ranges: */
	darray_foreach(i, *r) {
		t = tmp.size ?  &tmp.item[tmp.size - 1] : NULL;

		if (t && t->end >= i->start)
			t->end = max(t->end, i->end);
		else
			darray_append(tmp, *i);
	}

	darray_free(*r);
	*r = tmp;
}

void ranges_roundup(ranges *r, unsigned block_size)
{
	struct range *i;

	darray_foreach(i, *r) {
		i->start = round_down(i->start, block_size);
		i->end	= round_up(i->end, block_size);
	}
}

void ranges_rounddown(ranges *r, unsigned block_size)
{
	struct range *i;

	darray_foreach(i, *r) {
		i->start = round_up(i->start, block_size);
		i->end	= round_down(i->end, block_size);
		i->end	= max(i->end, i->start);
	}
}

struct fiemap_extent fiemap_iter_next(struct fiemap_iter *iter)
{
	struct fiemap_extent e;

	BUG_ON(iter->idx > iter->f.fm_mapped_extents);

	if (iter->idx == iter->f.fm_mapped_extents) {
		xioctl(iter->fd, FS_IOC_FIEMAP, &iter->f);

		if (!iter->f.fm_mapped_extents)
			return (struct fiemap_extent) { .fe_length = 0 };

		iter->idx = 0;
	}

	e = iter->f.fm_extents[iter->idx++];
	BUG_ON(!e.fe_length);

	iter->f.fm_start = e.fe_logical + e.fe_length;

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

unsigned hatoi_validate(const char *s, const char *msg)
{
	u64 v;

	if (bch2_strtoull_h(s, &v))
		die("bad %s %s", msg, s);

	v /= 512;

	if (v > USHRT_MAX)
		die("%s too large\n", msg);

	if (!v)
		die("%s too small\n", msg);

	return v;
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

struct mntent *dev_to_mount(char *dev)
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

int dev_mounted(char *dev)
{
	struct mntent *mnt = dev_to_mount(dev);

	if (!mnt)
		return 0;
	if (hasmntopt(mnt, "ro"))
		return 1;
	return 2;
}

struct bpos bpos_parse(char *buf)
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
