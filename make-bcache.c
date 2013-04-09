#define _FILE_OFFSET_BITS	64
#define __USE_FILE_OFFSET64
#define _XOPEN_SOURCE 600

#include <ctype.h>
#include <errno.h>
#include <fcntl.h>
#include <getopt.h>
#include <limits.h>
#include <linux/fs.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/types.h>
#include <sys/stat.h>
#include <unistd.h>
#include <uuid/uuid.h>

#include "bcache.h"

uint64_t getblocks(int fd)
{
	uint64_t ret;
	struct stat statbuf;
	if (fstat(fd, &statbuf)) {
		perror("stat error\n");
		exit(EXIT_FAILURE);
	}
	ret = statbuf.st_size / 512;
	if (S_ISBLK(statbuf.st_mode))
		if (ioctl(fd, BLKGETSIZE, &ret)) {
			perror("ioctl error");
			exit(EXIT_FAILURE);
		}
	return ret;
}

uint64_t hatoi(const char *s)
{
	char *e;
	long long i = strtoll(s, &e, 10);
	switch (*e) {
		case 't':
		case 'T':
			i *= 1024;
		case 'g':
		case 'G':
			i *= 1024;
		case 'm':
		case 'M':
			i *= 1024;
		case 'k':
		case 'K':
			i *= 1024;
	}
	return i;
}

unsigned hatoi_validate(const char *s, const char *msg)
{
	uint64_t v = hatoi(s);

	if (v & (v - 1)) {
		printf("%s must be a power of two\n", msg);
		exit(EXIT_FAILURE);
	}

	v /= 512;

	if (v > USHRT_MAX) {
		printf("%s too large\n", msg);
		exit(EXIT_FAILURE);
	}

	if (!v) {
		printf("%s too small\n", msg);
		exit(EXIT_FAILURE);
	}

	return v;
}

char *skip_spaces(const char *str)
{
	while (isspace(*str))
		++str;
	return (char *)str;
}

char *strim(char *s)
{
	size_t size;
	char *end;

	s = skip_spaces(s);
	size = strlen(s);
	if (!size)
		return s;

	end = s + size - 1;
	while (end >= s && isspace(*end))
		end--;
	*(end + 1) = '\0';

	return s;
}

ssize_t read_string_list(const char *buf, const char * const list[])
{
	size_t i;
	char *s, *d = strdup(buf);
	if (!d)
		return -ENOMEM;

	s = strim(d);

	for (i = 0; list[i]; i++)
		if (!strcmp(list[i], s))
			break;

	free(d);

	if (!list[i])
		return -EINVAL;

	return i;
}

void usage()
{
	printf("Usage: make-bcache [options] device\n"
	       "	-C, --cache		Format a cache device\n"
	       "	-B, --bdev		Format a backing device\n"
	       "	-b, --bucket		bucket size\n"
	       "	-w, --block		block size (hard sector size of SSD, often 2k)\n"
//	       "	-U			UUID\n"
	       "	    --writeback		enable writeback\n"
	       "	    --discard		enable discards\n"
	       "	    --cache_replacement_policy=(lru|fifo)\n"
	       "	-h, --help		display this help and exit\n");
	exit(EXIT_FAILURE);
}

const char * const cache_replacement_policies[] = {
	"lru",
	"fifo",
	"random",
	NULL
};

static void write_sb(char *dev, unsigned block_size, unsigned bucket_size,
		     bool writeback, bool discard,
		     unsigned cache_replacement_policy,
		     uint64_t data_offset,
		     uuid_t set_uuid, bool bdev)
{
	int fd;
	char uuid_str[40], set_uuid_str[40];
	struct cache_sb sb;

	memset(&sb, 0, sizeof(struct cache_sb));

	sb.version	= bdev ? CACHE_BACKING_DEV : 0;
	sb.bucket_size	= bucket_size;
	sb.block_size	= block_size;

	uuid_generate(sb.uuid);
	memcpy(sb.set_uuid, set_uuid, sizeof(sb.set_uuid));

	if (SB_BDEV(&sb)) {
		SET_BDEV_WRITEBACK(&sb, writeback);

		if (data_offset != BDEV_DATA_START) {
			sb.version = BCACHE_SB_BDEV_VERSION;
			sb.keys = 1;
			sb.d[0] = data_offset;
		}
	} else {
		SET_CACHE_DISCARD(&sb, discard);
		SET_CACHE_REPLACEMENT(&sb, cache_replacement_policy);
	}

	if ((fd = open(dev, O_RDWR|O_EXCL)) == -1) {
		printf("Can't open dev %s: %s\n", dev, strerror(errno));
		exit(EXIT_FAILURE);
	}

	sb.offset		= SB_SECTOR;
	memcpy(sb.magic, bcache_magic, 16);
	sb.nbuckets		= getblocks(fd) / sb.bucket_size;
	sb.nr_in_set		= 1;
	sb.first_bucket		= (23 / sb.bucket_size) + 1;
	sb.csum = csum_set(&sb);

	if (sb.nbuckets < 1 << 7) {
		printf("Not enough buckets: %ju, need %u\n",
		       sb.nbuckets, 1 << 7);
		exit(EXIT_FAILURE);
	}

	uuid_unparse(sb.uuid, uuid_str);
	uuid_unparse(sb.set_uuid, set_uuid_str);

	printf("UUID:			%s\n"
	       "Set UUID:		%s\n"
	       "version:		%u\n"
	       "nbuckets:		%ju\n"
	       "block_size:		%u\n"
	       "bucket_size:		%u\n"
	       "nr_in_set:		%u\n"
	       "nr_this_dev:		%u\n"
	       "first_bucket:		%u\n",
	       uuid_str, set_uuid_str,
	       (unsigned) sb.version,
	       sb.nbuckets,
	       sb.block_size,
	       sb.bucket_size,
	       sb.nr_in_set,
	       sb.nr_this_dev,
	       sb.first_bucket);

	if (pwrite(fd, &sb, sizeof(sb), SB_SECTOR << 9) != sizeof(sb)) {
		perror("write error\n");
		exit(EXIT_FAILURE);
	}

	fsync(fd);
	close(fd);
}

int main(int argc, char **argv)
{
	int c, bdev = -1;
	unsigned i, ncache_devices = 0, nbacking_devices = 0;
	char *cache_devices[argc];
	char *backing_devices[argc];

	unsigned block_size = 1, bucket_size = 1024;
	int writeback = 0, discard = 0;
	unsigned cache_replacement_policy = 0;
	uint64_t data_offset = BDEV_DATA_START;
	uuid_t set_uuid;

	uuid_generate(set_uuid);

	struct option opts[] = {
		{ "cache",		0, NULL,	'C' },
		{ "bdev",		0, NULL,	'B' },
		{ "bucket",		1, NULL,	'b' },
		{ "block",		1, NULL,	'w' },
		{ "writeback",		0, &writeback,	1 },
		{ "discard",		0, &discard,	1 },
		{ "cache_replacement_policy", 1, NULL, 'p' },
		{ "data_offset",	1, NULL,	'o' },
		{ "help",		0, NULL,	'h' },
		{ NULL,			0, NULL,	0 },
	};

	while ((c = getopt_long(argc, argv,
				"-hCBU:w:b:",
				opts, NULL)) != -1)
		switch (c) {
		case 'C':
			bdev = 0;
			break;
		case 'B':
			bdev = 1;
			break;
		case 'b':
			bucket_size = hatoi_validate(optarg, "bucket size");
			break;
		case 'w':
			block_size = hatoi_validate(optarg, "block size");
			break;
#if 0
		case 'U':
			if (uuid_parse(optarg, sb.uuid)) {
				printf("Bad uuid\n");
				exit(EXIT_FAILURE);
			}
			break;
#endif
		case 'p':
			cache_replacement_policy = read_string_list(optarg,
						    cache_replacement_policies);
			break;
		case 'o':
			data_offset = atoll(optarg);
			if (data_offset < BDEV_DATA_START) {
				printf("Bad data offset; minimum %d sectors\n", BDEV_DATA_START);
				exit(EXIT_FAILURE);
			}
			break;
		case 'h':
			usage();
			break;
		case 1:
			if (bdev == -1) {
				printf("Please specify -C or -B\n");
				exit(EXIT_FAILURE);
			}

			if (bdev)
				backing_devices[nbacking_devices++] = optarg;
			else
				cache_devices[ncache_devices++] = optarg;
			break;
		}

	if (!ncache_devices && !nbacking_devices) {
		printf("Please supply a device\n");
		usage();
	}

	if (bucket_size < block_size) {
		printf("Bucket size cannot be smaller than block size\n");
		exit(EXIT_FAILURE);
	}

	for (i = 0; i < ncache_devices; i++)
		write_sb(cache_devices[i], block_size, bucket_size,
			 writeback, discard, cache_replacement_policy,
			 data_offset, set_uuid, false);

	for (i = 0; i < nbacking_devices; i++)
		write_sb(backing_devices[i], block_size, bucket_size,
			 writeback, discard, cache_replacement_policy,
			 data_offset, set_uuid, true);

	return 0;
}
