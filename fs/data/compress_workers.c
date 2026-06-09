// SPDX-License-Identifier: GPL-2.0

#include "bcachefs.h"

#include "data/compress.h"
#include "data/compress_workers.h"

#include "closure.h"

#include <linux/slab.h>

static void bch2_compress_work_fn(struct work_struct *work)
{
	struct bch_compress_work *w =
		container_of(work, struct bch_compress_work, work);

	w->compression_type = bch2_compress_locked(
		w->cwq->c,
		w->dst, &w->dst_len,
		(void *) w->src, &w->src_len,
		w->compression_opt,
		w->write_pos,
		w->worker->workspace,
		w->worker->verify_buf);

	closure_put(w->parent);
}

int bch2_compress_wq_init(struct bch_fs *c)
{
	unsigned nr = bch2_compress_nr_workers();
	struct bch_compress_wq *cwq;
	int ret = 0;

	cwq = kzalloc(sizeof(*cwq), GFP_KERNEL);
	if (!cwq)
		return -ENOMEM;

	cwq->c = c;
	cwq->nr_workers = nr;

	cwq->wq = alloc_workqueue("bcachefs-compress",
				  WQ_UNBOUND | WQ_HIGHPRI, nr);
	if (!cwq->wq) {
		ret = -ENOMEM;
		goto err_free_cwq;
	}

	cwq->workers = kcalloc(nr, sizeof(*cwq->workers), GFP_KERNEL);
	if (!cwq->workers) {
		ret = -ENOMEM;
		goto err_destroy_wq;
	}

	size_t ws_size = READ_ONCE(c->compress.zstd_workspace_size);
	unsigned extent_max = c->opts.encoded_extent_max;

	for (unsigned i = 0; i < nr; i++) {
		struct bch_compress_worker *worker = &cwq->workers[i];

		worker->workspace = kvzalloc(ws_size, GFP_KERNEL);
		if (!worker->workspace) {
			ret = -ENOMEM;
			goto err_free_workers;
		}

		worker->dst_buf = kvzalloc(extent_max, GFP_KERNEL);
		if (!worker->dst_buf) {
			ret = -ENOMEM;
			goto err_free_workers;
		}

		worker->verify_buf = kvzalloc(extent_max, GFP_KERNEL);
		if (!worker->verify_buf) {
			ret = -ENOMEM;
			goto err_free_workers;
		}
	}

	c->compress.mt_wq = cwq;
	return 0;
err_free_workers:
	for (unsigned i = 0; i < nr; i++) {
		kvfree(cwq->workers[i].workspace);
		kvfree(cwq->workers[i].dst_buf);
		kvfree(cwq->workers[i].verify_buf);
	}
	kfree(cwq->workers);
err_destroy_wq:
	destroy_workqueue(cwq->wq);
err_free_cwq:
	kfree(cwq);
	return ret;
}

void bch2_compress_wq_destroy(struct bch_fs *c)
{
	struct bch_compress_wq *cwq = c->compress.mt_wq;

	if (!cwq)
		return;

	c->compress.mt_wq = NULL;

	for (unsigned i = 0; i < cwq->nr_workers; i++) {
		kvfree(cwq->workers[i].workspace);
		kvfree(cwq->workers[i].dst_buf);
		kvfree(cwq->workers[i].verify_buf);
	}
	kfree(cwq->workers);
	destroy_workqueue(cwq->wq);
	kfree(cwq);
}

void bch2_compress_wq_submit(struct bch_compress_work *w,
			     struct bch_compress_wq *cwq,
			     struct closure *parent,
			     unsigned compression_opt,
			     struct bpos write_pos,
			     const void *src, size_t src_len,
			     void *dst, size_t dst_len,
			     struct bch_compress_worker *worker)
{
	INIT_WORK(&w->work, bch2_compress_work_fn);
	w->cwq = cwq;
	w->compression_opt = compression_opt;
	w->write_pos = write_pos;
	w->src = src;
	w->src_len = src_len;
	w->dst = dst;
	w->dst_len = dst_len;
	w->compression_type = 0;
	w->worker = worker;

	closure_get(parent);
	w->parent = parent;
	queue_work(cwq->wq, &w->work);
}
