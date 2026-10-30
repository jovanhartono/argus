/* The hatch between an Argus window and the Rust engine. Written by hand to match
   crates/argus-ffi/src/lib.rs, which explains each item; a test fails when they differ. */
#ifndef ARGUS_H
#define ARGUS_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

static const uint32_t TL_ABI_VERSION = 14;

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
static const uint32_t TL_EVENT_RERANKED = 8;

typedef struct TlCore TlCore;

typedef void (*TlEventFn)(void *ctx, uint32_t event, uint64_t value);

typedef struct TlRow {
    uint32_t name_start;
    uint32_t name_len;
    uint32_t folder_start;
    uint32_t folder_len;
    bool is_folder;
    bool is_app;
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

typedef struct TlHealth {
    uint64_t entries;
    uint64_t scanned_at;
    uint64_t scan_ms;
    uint64_t changed_at;
    bool busy;
    bool building;
    bool behind;
    bool stopped;
} TlHealth;

uint32_t tl_abi_version(void);
TlCore *tl_core_new(const uint8_t *root, size_t root_len, uint32_t full_disk, uint32_t everything, const TlSaved *saved, void *ctx, TlEventFn on_event);
TlCore *tl_core_new_stand_in(uint64_t count, void *ctx, TlEventFn on_event);
uint64_t tl_submit(const TlCore *core, const uint8_t *query, size_t len, const uint8_t *folder, size_t folder_len);
uint32_t tl_apps(const TlCore *core, const uint8_t *folders, size_t len);
uint32_t tl_changed(const TlCore *core, const uint8_t *paths, size_t len, uint32_t rescan, uint64_t mark);
void tl_grant_full_disk(const TlCore *core, uint32_t everything, uint64_t mark);
uint32_t tl_index_everything(const TlCore *core, uint32_t on, uint64_t mark);
uint32_t tl_window_shown(const TlCore *core, uint32_t shown);
uint32_t tl_volume_added(const TlCore *core, const uint8_t *path, size_t len, uint64_t mark);
uint32_t tl_volume_removed(const TlCore *core, const uint8_t *path, size_t len, uint64_t mark);
void tl_volume_gone(const TlCore *core, const uint8_t *path, size_t len);
uint32_t tl_watched(const TlCore *core, TlRows *out, uint64_t *since);
uint32_t tl_health(const TlCore *core, TlHealth *out);
uint32_t tl_holds_folder(const TlCore *core, const uint8_t *path, size_t len);
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
