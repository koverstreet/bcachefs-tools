#include <assert.h>
#include <malloc.h>
#include <signal.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

void trick_compiler(int *x);

static void test_abort(void)
{
	abort();
}

static void test_segfault(void)
{
	raise(SIGSEGV);
}

static void test_leak(void)
{
	int *p = malloc(sizeof *p);
	trick_compiler(p);
}

static void test_undefined(void)
{
	int *p = malloc(1);
	printf("%d\n", *p);
}

static void test_undefined_branch(void)
{
	int x;	
	trick_compiler(&x);

	if (x)
		printf("1\n");
	else
		printf("0\n");
}

static void test_read_after_free(void)
{
	int *p = malloc(sizeof *p);
	free(p);

	printf("%d\n", *p);
}

static void test_write_after_free(void)
{
	int *p = malloc(sizeof *p);
	free(p);

	printf("%d\n", *p);
}

typedef void (*test_fun)(void);

struct test {
	const char	*name;
	test_fun 	fun;
};

#define TEST(f) { .name = #f, .fun = test_##f, }
static struct test tests[] = {
	TEST(abort),
	TEST(segfault),
	TEST(leak),
	TEST(undefined),
	TEST(undefined_branch),
	TEST(read_after_free),
	TEST(write_after_free),
};
#define ntests (sizeof tests / sizeof *tests)

int main(int argc, char *argv[])
{
	int i;

	if (argc != 2) {
		fprintf(stderr, "Usage: test_helper <test>\n");
		exit(1);
	}

	bool found = false;
	for (i = 0; i < ntests; ++i)
		if (!strcmp(argv[1], tests[i].name)) {
			found = true;
			printf("Running test: %s\n", tests[i].name);
			tests[i].fun();
			break;
		}

	if (!found) {
		fprintf(stderr, "Unable to find test: %s\n", argv[1]);
		exit(1);
	}

	return 0;
}
