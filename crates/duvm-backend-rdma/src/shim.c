/* Thin shim to expose libibverbs inline functions to Rust FFI. */
#include <infiniband/verbs.h>

int duvm_ibv_post_send(struct ibv_qp *qp, struct ibv_send_wr *wr,
                       struct ibv_send_wr **bad_wr) {
    return ibv_post_send(qp, wr, bad_wr);
}

int duvm_ibv_poll_cq(struct ibv_cq *cq, int num_entries, struct ibv_wc *wc) {
    return ibv_poll_cq(cq, num_entries, wc);
}
