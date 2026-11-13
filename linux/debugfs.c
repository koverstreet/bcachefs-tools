#include <linux/debugfs.h>
#include <linux/err.h>
#include "tools-util.h"
#include "linux/darray.h"

#include "util/util.h"

static bool str_end_eq(const char *s1, const char *s2, const char *s2_end)
{
	return strlen(s1) == s2_end - s2 &&
		!memcmp(s1, s2, s2_end - s2);
}

struct debugfs_dentry {
	struct dentry			d;
	struct inode			i;
	const struct file_operations	*fops;

	DARRAY(struct debugfs_dentry *)	children;
};

static struct debugfs_dentry debugfs_root = (struct debugfs_dentry) {
	.d.is_debugfs	= true,
	.i.mode		= 0755|S_IFDIR,
};

/*
 * Defined in the tools binary (src/http.rs). A weak empty definition lets
 * other links of this archive, such as the bcachefs-kernel crate tests,
 * resolve the symbol. The tools binary's strong definition overrides it.
 */
__attribute__((weak)) void bch2_start_http_lazy(void) {}

struct dentry *debugfs_create_file(const char *name, umode_t mode,
				   struct dentry *d_parent, void *data,
				   const struct file_operations *fops)
{
	/*
	 * Only start the http server when an fs is actually being brought
	 * up. Module-init creates the root "bcachefs" debugfs dir (and the
	 * closure debug file) with d_parent == NULL; per-fs debugfs creates
	 * always pass a non-NULL parent. Without this guard every command
	 * (--help, version, ...) would bind the unix socket.
	 */
	if (d_parent)
		bch2_start_http_lazy();

	if (!d_parent)
		d_parent = &debugfs_root.d;

	BUG_ON(!d_parent->is_debugfs);
	struct debugfs_dentry *parent = container_of(d_parent, struct debugfs_dentry, d);

	struct debugfs_dentry *n = kzalloc(sizeof(*n), GFP_KERNEL);
	if (!n)
		return ERR_PTR(-ENOMEM);

	n->d.is_debugfs		= true;
	n->d.name		= strdup(name);
	n->i.mode		= mode;
	n->i.i_private		= data;
	n->fops			= fops;

	darray_push(&parent->children, n);

	return &n->d;
}

struct dentry *debugfs_create_dir(const char *name, struct dentry *parent)
{
	return debugfs_create_file(name, 0755|S_IFDIR, parent, NULL, NULL);
}

void debugfs_remove(struct dentry *dentry)
{ }

void debugfs_remove_recursive(struct dentry *dentry)
{ }

struct debugfs_dentry *debugfs_path_lookup(const char *path)
{
	struct debugfs_dentry *d = &debugfs_root;

	while (true) {
		while (*path == '/')
			path++;
		if (!*path)
			return d;

		const char *end = strchrnul(path, '/');

		struct debugfs_dentry **child =
			darray_find_p(d->children, i, str_end_eq((*i)->d.name, path, end));
		if (!child)
			return ERR_PTR(-ENOENT);

		d = *child;
		path = end;
	}
}

int debugfs_read_or_html_dirlist(const char *path, FILE *out)
{
	struct debugfs_dentry *d = errptr_try(debugfs_path_lookup(path));
	ssize_t ret = 0;

	if (S_ISDIR(d->i.mode)) {
		fputs("<html>\n", out);

		fprintf(out, "<p> %s </p>", path);

		darray_for_each(d->children, i)
			fprintf(out, "<p> <a href=%s/%s>%s</a></p>\n", path, (*i)->d.name, (*i)->d.name);

		fputs("</html>\n", out);
	} else {
		struct file f = { .f_inode = &d->i };
		try(d->fops->open(&d->i, &f));

		char buf[PAGE_SIZE];
		loff_t pos = 0;
		while (true) {
			ret = d->fops->read(&f, buf, PAGE_SIZE, &pos);
			if (ret <= 0)
				break;

			fwrite(buf, 1, ret, out);
		}

		d->fops->release(&d->i, &f);
	}

	return ret;
}
