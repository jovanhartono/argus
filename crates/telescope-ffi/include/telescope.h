/* The hatch between a Telescope window and the Rust engine. Written by hand to match
   crates/telescope-ffi/src/lib.rs, which explains each item; a test fails when they differ. */
#ifndef TELESCOPE_H
#define TELESCOPE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

static const uint32_t TL_ABI_VERSION = 1;

static const uint32_t TL_OK = 0;
static const uint32_t TL_GONE = 1;
static const uint32_t TL_PANICKED = 2;

static const uint32_t TL_EVENT_INDEXED = 1;
static const uint32_t TL_EVENT_READY = 2;
static const uint32_t TL_EVENT_FAILED = 3;

typedef struct TlCore TlCore;

typedef void (*TlEventFn)(void *ctx, uint32_t event, uint64_t value);

typedef struct TlRow {
    uint32_t name_start;
    uint32_t name_len;
    uint32_t folder_start;
    uint32_t folder_len;
    bool is_folder;
} TlRow;

typedef struct TlRows {
    const TlRow *rows;
    size_t count;
    const uint8_t *text;
    void *owner;
} TlRows;

uint32_t tl_abi_version(void);
TlCore *tl_core_new_stand_in(uint64_t count, void *ctx, TlEventFn on_event);
uint64_t tl_submit(const TlCore *core, const uint8_t *query, size_t len);
uint32_t tl_take(const TlCore *core, uint64_t ticket, TlRows *out);
void tl_rows_free(TlRows rows);
void tl_core_free(TlCore *core);

#endif
