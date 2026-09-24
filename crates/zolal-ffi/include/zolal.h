/*
 * Zolal engine — C ABI.
 *
 * Hides encrypted files inside JPEG, MP4 and PDF carriers. This header is the canonical
 * description of the boundary; the Zolal app's ZolalKit wraps it in a Swift API, and
 * `crates/zolal-ffi/src/lib.rs` implements it.
 *
 * Rules that apply everywhere:
 *   - Paths cross as NUL-terminated byte strings, exactly as the filesystem stores them.
 *     File *contents* never cross: a 4 GB video is streamed from disk, never marshalled.
 *   - Every call blocks. Run them off the main thread.
 *   - Anything the library hands back, the library frees: zolal_error_free,
 *     zolal_reveal_report_free, zolal_progress_free.
 *   - `err` may be NULL if you do not want details. When it is not NULL, pass it to
 *     zolal_error_free afterwards, even on success.
 *   - Nothing panics across this boundary; an internal fault becomes ZOLAL_ERR_INTERNAL.
 */

#ifndef ZOLAL_H
#define ZOLAL_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* --- status codes ------------------------------------------------------------------------ */

#define ZOLAL_OK                        0
/* A region that could hold a payload exists, but the passphrase did not authenticate it.
 * Note it can also mean the file simply carries unrelated trailing bytes. */
#define ZOLAL_ERR_WRONG_PASSPHRASE      1
/* Nothing hidden here. See ZolalErrorDetail.looks_reencoded. */
#define ZOLAL_ERR_NO_HIDDEN_DATA        2
/* Right passphrase, damaged or truncated payload. Nothing was written. */
#define ZOLAL_ERR_DAMAGED_PAYLOAD       3
/* Not a JPEG, MP4 or PDF; the message names what was detected. Usually means the import
 * pipeline failed to convert a HEIC or MOV first. */
#define ZOLAL_ERR_UNSUPPORTED_FORMAT    4
/* Would exceed the format's size limit; size/limit/suggestion are filled in. */
#define ZOLAL_ERR_PAYLOAD_TOO_LARGE     5
#define ZOLAL_ERR_CARRIER_TOO_SMALL     6
#define ZOLAL_ERR_MALFORMED_CONTAINER   7
#define ZOLAL_ERR_CORRUPT_BUNDLE        8
#define ZOLAL_ERR_INVALID_REQUEST       9
#define ZOLAL_ERR_CANCELLED            10
#define ZOLAL_ERR_IO                   11
#define ZOLAL_ERR_INTERNAL             12

/* --- enumerations ------------------------------------------------------------------------ */

#define ZOLAL_FORMAT_JPEG               0
#define ZOLAL_FORMAT_MP4                1
#define ZOLAL_FORMAT_PDF                2
#define ZOLAL_FORMAT_MP3                3

#define ZOLAL_TECHNIQUE_AUTO            0  /* pick the default for the detected format */
#define ZOLAL_TECHNIQUE_JPEG_TRAILER    1
#define ZOLAL_TECHNIQUE_JPEG_APP15      2
#define ZOLAL_TECHNIQUE_MP4_FREE_BOX    3
#define ZOLAL_TECHNIQUE_PDF_OBJECT      4
#define ZOLAL_TECHNIQUE_MP3_ID3         5

#define ZOLAL_VERDICT_NATURAL           0
#define ZOLAL_VERDICT_NOTICEABLE        1
#define ZOLAL_VERDICT_SUSPICIOUS        2
#define ZOLAL_VERDICT_TOO_LARGE         3  /* hiding will be refused; suggest a video */

/* --- structures -------------------------------------------------------------------------- */

typedef struct {
    char    *message;          /* owned; NULL when there is none */
    bool     looks_reencoded;  /* ZOLAL_ERR_NO_HIDDEN_DATA */
    uint64_t size;             /* ZOLAL_ERR_PAYLOAD_TOO_LARGE: what it would have weighed */
    uint64_t limit;            /* ZOLAL_ERR_PAYLOAD_TOO_LARGE: the limit exceeded */
    uint8_t  suggestion;       /* ZOLAL_ERR_PAYLOAD_TOO_LARGE: ZOLAL_FORMAT_* to use instead */
} ZolalErrorDetail;

typedef struct {
    uint8_t  format;                /* ZOLAL_FORMAT_* */
    uint64_t file_size;
    bool     has_candidate_region;  /* could hold something; only a passphrase confirms */
} ZolalProbe;

typedef struct {
    uint64_t result_size;
    float    ratio;             /* result_size / original carrier size */
    uint8_t  verdict;           /* ZOLAL_VERDICT_* */
} ZolalPlausibility;

typedef struct {
    uint64_t          output_size;
    uint8_t           technique;    /* ZOLAL_TECHNIQUE_*, with AUTO resolved */
    ZolalPlausibility plausibility;
} ZolalHideReport;

typedef struct {
    uint64_t output_size;           /* size of the cleaned file */
    uint64_t removed_bytes;         /* how much the carrier shed */
    uint8_t  technique;             /* ZOLAL_TECHNIQUE_*: where the payload had been */
} ZolalCleanReport;

typedef struct ZolalRevealReport ZolalRevealReport;  /* opaque */
typedef struct ZolalProgress     ZolalProgress;      /* opaque */

/* --- operations -------------------------------------------------------------------------- */

/* Identify a file and say whether it could hold (or already holds) a payload. */
int32_t zolal_probe(const char *path, ZolalProbe *out, ZolalErrorDetail *err);

/* Predict how natural the result would look, before committing to anything.
 * A ZOLAL_VERDICT_TOO_LARGE answer means zolal_hide will refuse. */
int32_t zolal_plausibility(const char *carrier,
                           uint64_t payload_bytes,
                           ZolalPlausibility *out,
                           ZolalErrorDetail *err);

/* Hide payload_count files inside `carrier`, writing to `output`.
 *
 * The passphrase must already be Unicode NFC (Swift: precomposedStringWithCanonicalMapping),
 * or the same phrase typed on another device derives a different key.
 * `progress` may be NULL; it must outlive the call. */
int32_t zolal_hide(const char *carrier,
                   const char *const *payloads,
                   size_t payload_count,
                   const char *output,
                   const char *passphrase,
                   uint8_t technique,
                   const ZolalProgress *progress,
                   ZolalHideReport *out,
                   ZolalErrorDetail *err);

/* Write `carrier` back out to `output` without its hidden content, so it is an ordinary file of
 * its format again. Needs the passphrase: hiding is markerless, so a region is only known to be
 * ours once its tag authenticates. ZOLAL_ERR_NO_HIDDEN_DATA if there is no plausible region,
 * ZOLAL_ERR_WRONG_PASSPHRASE if none authenticates. On failure nothing is written. */
int32_t zolal_clean(const char *carrier,
                    const char *output,
                    const char *passphrase,
                    const ZolalProgress *progress,
                    ZolalCleanReport *out,
                    ZolalErrorDetail *err);

/* Recover hidden files into `output_dir`. On failure nothing is left there.
 * On success, free *out with zolal_reveal_report_free. */
int32_t zolal_reveal(const char *carrier,
                     const char *output_dir,
                     const char *passphrase,
                     const ZolalProgress *progress,
                     ZolalRevealReport **out,
                     ZolalErrorDetail *err);

/* --- reveal report ----------------------------------------------------------------------- */

size_t      zolal_reveal_report_file_count(const ZolalRevealReport *report);
/* Borrowed; valid until the report is freed. NULL if index is out of range. */
const char *zolal_reveal_report_file(const ZolalRevealReport *report, size_t index);
uint64_t    zolal_reveal_report_total_bytes(const ZolalRevealReport *report);
uint8_t     zolal_reveal_report_technique(const ZolalRevealReport *report);
void        zolal_reveal_report_free(ZolalRevealReport *report);

/* --- progress and cancellation ------------------------------------------------------------ */

ZolalProgress *zolal_progress_new(void);
/* Latest counts. `total` is 0 until known, so show an indeterminate spinner until then.
 * Either out-pointer may be NULL. Safe to call from another thread while work runs. */
void zolal_progress_read(const ZolalProgress *handle, uint64_t *done, uint64_t *total);
/* Stop at the next chunk boundary with ZOLAL_ERR_CANCELLED, leaving nothing behind. */
void zolal_progress_cancel(const ZolalProgress *handle);
/* Not while an operation using it is still running. */
void zolal_progress_free(ZolalProgress *handle);

/* --- errors ------------------------------------------------------------------------------- */

/* Frees the message and re-zeroes the struct. Safe on a zeroed struct. */
void zolal_error_free(ZolalErrorDetail *err);

#ifdef __cplusplus
}
#endif

#endif /* ZOLAL_H */
