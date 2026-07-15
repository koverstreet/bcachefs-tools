#include <linux/kobject.h>
#include <linux/mutex.h>
#include "../c_src/tools-util.h"

#include "util/printbuf.h"
#include "util/util.h"

static DEFINE_MUTEX(kobj_lock);

static void kobject_cleanup(struct kobject *);

static struct kobject *root_kobj;

void kobject_init(struct kobject *kobj, const struct kobj_type *ktype)
{
	memset(kobj, 0, sizeof(*kobj));

	atomic_set(&kobj->ref, 1);
	kobj->ktype = ktype;
	kobj->state_initialized = 1;
}

int kobject_add(struct kobject *kobj, struct kobject *parent,
		const char *fmt, ...)
{
	va_list args;
	va_start(args, fmt);
	kobj->name = vmprintf(fmt, args);
	va_end(args);

	if (kobj->ktype->default_groups &&
	    kobj->ktype->default_groups[0] &&
	    kobj->ktype->default_groups[0]->attrs) {
		struct attribute **attr = kobj->ktype->default_groups[0]->attrs;

		while (*attr) {
			darray_push(&kobj->files, *attr);
			attr++;
		}
	}

	scoped_guard(mutex, &kobj_lock) {
		if (parent) {
			darray_push(&parent->subdirs, kobj);
			kobj->parent = kobject_get(parent);
		} else {
			BUG_ON(root_kobj);
			root_kobj = kobj;
		}
	}

	kobj->state_in_sysfs = true;

	return 0;
}

void kobject_del(struct kobject *kobj)
{
	if (!kobj)
		return;

	if (kobj->state_in_sysfs) {
		guard(mutex)(&kobj_lock);

		if (kobj->parent) {
			struct kobject **i = darray_find(kobj->parent->subdirs, kobj);
			if (i) {
				darray_remove_item(&kobj->parent->subdirs, i);
			} else {
				WARN(1, "child %s not found in parent %s with %zu subdirs when deleting kobj",
				     kobj->name, kobj->parent->name, kobj->parent->subdirs.nr);
			}
		} else {
			BUG_ON(kobj != root_kobj);
			root_kobj = NULL;
		}
	}

	kobj->state_in_sysfs = false;

	if (kobj->parent)
		kobject_put(kobj->parent);
	kobj->parent = NULL;
}

static void kobject_cleanup(struct kobject *kobj)
{
	const struct kobj_type *t = kobj->ktype;

	/* remove from sysfs if the caller did not do it */
	if (kobj->state_in_sysfs)
		kobject_del(kobj);

	darray_exit(&kobj->files);
	darray_exit(&kobj->bin_files);
	darray_exit(&kobj->subdirs);

	if (t && t->release)
		t->release(kobj);
}

void kobject_put(struct kobject *kobj)
{
	BUG_ON(!kobj);
	BUG_ON(!kobj->state_initialized);

	if (atomic_dec_and_test(&kobj->ref))
		kobject_cleanup(kobj);
}

struct kobject *kobject_get(struct kobject *kobj)
{
	BUG_ON(!kobj);
	BUG_ON(!kobj->state_initialized);

	atomic_inc(&kobj->ref);
	return kobj;
}

int sysfs_create_file(struct kobject *kobj, const struct attribute *attr)
{
	guard(mutex)(&kobj_lock);
	return darray_push(&kobj->files, attr);
}

int sysfs_create_bin_file(struct kobject *kobj, const struct bin_attribute *attr)
{
	guard(mutex)(&kobj_lock);
	return darray_push(&kobj->bin_files, attr);
}

void sysfs_remove_bin_file(struct kobject *kobj, const struct bin_attribute *attr)
{
	guard(mutex)(&kobj_lock);
	const struct bin_attribute **i = darray_find(kobj->bin_files, attr);
	if (i)
		darray_remove_item(&kobj->bin_files, i);
}

static bool str_end_eq(const char *s1, const char *s2, const char *s2_end)
{
	return strlen(s1) == s2_end - s2 &&
		!memcmp(s1, s2, s2_end - s2);
}

struct attribute *path_lookup(const char *path, struct kobject **_dir,
			     const struct bin_attribute **_bin_attr)
{
	struct kobject *dir = root_kobj;
	if (!dir)
		return ERR_PTR(-ENOENT);

	if (_bin_attr)
		*_bin_attr = NULL;

	while (true) {
		*_dir = dir;

		while (*path == '/')
			path++;
		if (!*path)
			break;

		const char *end = strchrnul(path, '/');

		if (!*end) {
			struct attribute **attr = (struct attribute **)
				darray_find_p(dir->files, i, str_end_eq((*i)->name, path, end));
			if (attr)
				return *attr;

			const struct bin_attribute **battr =
				darray_find_p(dir->bin_files, i, str_end_eq((*i)->attr.name, path, end));
			if (battr) {
				if (_bin_attr)
					*_bin_attr = *battr;
				return (struct attribute *)&(*battr)->attr;
			}
		}

		struct kobject **child =
			darray_find_p(dir->subdirs, i, str_end_eq((*i)->name, path, end));
		if (!child)
			return ERR_PTR(-ENOENT);

		dir = *child;
		path = end;
	}

	return NULL;
}

static int debugfs_read_or_html_dirlist(const char *, struct printbuf *);

int sysfs_read_or_html_dirlist(const char *path, struct printbuf *out)
{
	guard(mutex)(&kobj_lock);

	if (!strcmp(path, "debug") ||
	    (strlen(path) > 5 &&
	     !memcmp(path, "debug/", 6)))
		return debugfs_read_or_html_dirlist(path + 5, out);

	struct kobject *dir;
	const struct bin_attribute *bin_attr;
	struct attribute *attr = errptr_try(path_lookup(path, &dir, &bin_attr));
	if (bin_attr) {
		loff_t off = 0;
		while (true) {
			try(bch2_printbuf_make_room(out, PAGE_SIZE));

			ssize_t ret = bin_attr->read(NULL, dir, bin_attr,
						     &out->buf[out->pos],
						     off, PAGE_SIZE);
			if (ret <= 0)
				break;

			BUG_ON(ret > printbuf_remaining(out));
			out->pos += ret;
			off += ret;
			printbuf_nul_terminate(out);
		}
		return 0;
	} else if (!attr) {
		prt_str(out, "<html>\n");

		prt_printf(out, "<p> %s </p>", path);

		darray_for_each(dir->subdirs, i)
			prt_printf(out, "<p> <a href=%s/%s>%s/</a></p>\n", path, (*i)->name, (*i)->name);

		if (dir == root_kobj)
			prt_printf(out, "<p> <a href=debug>debug/</a></p>\n");

		prt_printf(out, "<p> %zu subdirs </p>\n", dir->subdirs.nr);

		darray_for_each(dir->files, i)
			prt_printf(out, "<p> <a href=%s/%s>%s</a></p>\n", path, (*i)->name, (*i)->name);

		darray_for_each(dir->bin_files, i)
			prt_printf(out, "<p> <a href=%s/%s>%s</a></p>\n", path, (*i)->attr.name, (*i)->attr.name);

		prt_printf(out, "<p> %zu files </p>\n", dir->files.nr + dir->bin_files.nr);

		prt_str(out, "</html>\n");
		return 0;
	} else {
		try(bch2_printbuf_make_room(out, PAGE_SIZE));

		int ret = dir->ktype->sysfs_ops->show(dir, attr, out->buf);
		if (ret < 0)
			return ret;
		out->pos = ret;
		return 0;
	}
}

int sysfs_write(const char *path, const char *buf, size_t len)
{
	BUG_ON(len > PAGE_SIZE);

	guard(mutex)(&kobj_lock);

	struct kobject *dir;
	const struct bin_attribute *bin_attr;
	struct attribute *attr = errptr_try(path_lookup(path, &dir, &bin_attr));
	if (bin_attr) {
		if (!bin_attr->write)
			return -EACCES;
		return bin_attr->write(NULL, dir, bin_attr, (char *)buf, 0, len);
	}
	if (!attr)
		return -ENOENT;

	return dir->ktype->sysfs_ops->store(dir, attr, buf, len);
}


#include <linux/err.h>

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

extern void bch2_start_http_lazy(void);

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

static int debugfs_read_or_html_dirlist(const char *path, struct printbuf *out)
{
	struct debugfs_dentry *d = errptr_try(debugfs_path_lookup(path));
	ssize_t ret = 0;

	if (S_ISDIR(d->i.mode)) {
		prt_str(out, "<html>\n");

		prt_printf(out, "<p> %s </p>", path);

		darray_for_each(d->children, i)
			prt_printf(out, "<p> <a href=%s/%s>%s</a></p>\n", path, (*i)->d.name, (*i)->d.name);

		prt_str(out, "</html>\n");
	} else {
		struct file f = { .f_inode = &d->i };
		try(d->fops->open(&d->i, &f));

		loff_t pos = 0;
		while (true) {
			try(bch2_printbuf_make_room(out, PAGE_SIZE));

			ret = d->fops->read(&f, &out->buf[out->pos], printbuf_remaining(out), &pos);
			if (ret <= 0)
				break;

			BUG_ON(ret > printbuf_remaining(out));
			out->pos += ret;
			printbuf_nul_terminate(out);
		}

		d->fops->release(&d->i, &f);
	}

	return ret;
}
