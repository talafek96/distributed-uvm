/* duvm.h - C API for duvm distributed memory library */

#ifndef DUVM_H
#define DUVM_H

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

/**
 * Initialize the duvm library. Must be called before any other duvm function.
 * Returns 0 on success, -1 on error.
 */
int32_t duvm_init(void);

/**
 * Store a 4KB page. Returns a handle (u64) on success, or u64::MAX on error.
 *
 * # Safety
 * `data` must point to at least 4096 bytes of readable memory.
 */
uint64_t duvm_store_page(const uint8_t *data);

/**
 * Load a 4KB page by handle. Returns 0 on success, -1 on error.
 *
 * # Safety
 * `buf` must point to at least 4096 bytes of writable memory.
 */
int32_t duvm_load_page(uint64_t handle, uint8_t *buf);

/**
 * Free a page by handle. Returns 0 on success, -1 on error.
 */
int32_t duvm_free_page(uint64_t handle);

/**
 * Get total capacity in pages.
 */
uint64_t duvm_capacity_total(void);

/**
 * Get used capacity in pages.
 */
uint64_t duvm_capacity_used(void);

#endif  /* DUVM_H */
