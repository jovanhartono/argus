/* The hatch between a Telescope window and the Rust engine. Written by hand to match
   crates/telescope-ffi/src/lib.rs, which explains each item; a test fails when they differ. */
#ifndef TELESCOPE_H
#define TELESCOPE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

static const uint32_t TL_ABI_VERSION = 9;

static const uint32_t TL_OK = 0;
static const uint32_t TL_GONE = 1;
static const uint32_t TL_PANICKED = 2;

static const uint32_t TL_EVENT_INDEXED = 1;
static const uint32_t TL_EVENT_READY = 2;
static const uint32_t TL_EVENT_FAILED = 3;
static const uint32_t TL_EVENT_NOTE = 4;
static const uint32_t TL_EVENT_REBUILT = 5;
static const uint32_t TL_EVENT_LINES = 6;
static const uint32_t TL_EVENT_OPENED = 7;

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

typedef struct TlSaved {
    const uint8_t *file;
    size_t file_len;
    const uint8_t *journal;
    size_t journal_len;
    uint64_t now;
} TlSaved;

typedef struct TlLine {
    uint32_t path_start;
    uint32_t path_len;
    uint32_t text_start;
    uint32_t text_len;
    uint32_t match_start;
    uint32_t match_len;
    uint64_t line;
} TlLine;

typedef struct TlReport {
    uint64_t files_searched;
    uint64_t lines_found;
    uint64_t binary;
    uint64_t too_large;
    uint64_t cloud_only;
    uint64_t unreadable;
} TlReport;

typedef struct TlLines {
    const TlLine *lines;
    size_t count;
    const uint8_t *text;
    bool done;
    bool failed;
    TlReport report;
    void *owner;
} TlLines;

uint32_t tl_abi_version(void);
TlCore *tl_core_new(const uint8_t *root, size_t root_len, uint32_t full_disk, uint32_t everything, const TlSaved *saved, void *ctx, TlEventFn on_event);
TlCore *tl_core_new_stand_in(uint64_t count, void *ctx, TlEventFn on_event);
uint64_t tl_submit(const TlCore *core, const uint8_t *query, size_t len, const uint8_t *folder, size_t folder_len);
uint32_t tl_changed(const TlCore *core, const uint8_t *paths, size_t len, uint32_t rescan, uint64_t mark);
void tl_grant_full_disk(const TlCore *core, uint64_t mark);
uint32_t tl_index_everything(const TlCore *core, uint32_t on, uint64_t mark);
uint32_t tl_volume_added(const TlCore *core, const uint8_t *path, size_t len, uint64_t mark);
uint32_t tl_volume_removed(const TlCore *core, const uint8_t *path, size_t len, uint64_t mark);
uint32_t tl_watched(const TlCore *core, TlRows *out, uint64_t *since);
uint32_t tl_take(const TlCore *core, uint64_t ticket, TlRows *out);
void tl_rows_free(TlRows rows);
uint32_t tl_frecency_open(const TlCore *core, const uint8_t *file, size_t len);
uint32_t tl_opened(const TlCore *core, const uint8_t *path, size_t len);
uint64_t tl_grep_submit(const TlCore *core, const uint8_t *folders, size_t folders_len, const uint8_t *query, size_t query_len);
uint32_t tl_grep_take(const TlCore *core, uint64_t ticket, TlLines *out);
void tl_grep_cancel(const TlCore *core);
void tl_lines_free(TlLines lines);
void tl_core_free(TlCore *core);

#endif
