/* demod_bt.h - DeMoD BT C ABI for Haskell FFI
 *
 * LGPL-3.0 | Patent Pending | (c) 2025 DeMoD LLC
 *
 * Lifecycle:
 *   demod_bt_init -> demod_bt_register -> (poll events) ->
 *   demod_bt_acquire_and_start -> (streaming) ->
 *   demod_bt_stop_stream -> demod_bt_shutdown
 */

#ifndef DEMOD_BT_H
#define DEMOD_BT_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ── Metrics ──────────────────────────────────────────────────── */

typedef struct {
    uint32_t frames_processed;
    uint32_t underruns;
    uint32_t overruns;
    uint32_t buffer_level;
    uint8_t  running;
} demod_bt_metrics_t;

/* ── Events ───────────────────────────────────────────────────── */

#define DEMOD_BT_EVT_NONE                0
#define DEMOD_BT_EVT_DEVICE_CONNECTED    1
#define DEMOD_BT_EVT_DEVICE_DISCONNECTED 2
#define DEMOD_BT_EVT_TRANSPORT_ACQUIRED  3
#define DEMOD_BT_EVT_TRANSPORT_RELEASED  4
#define DEMOD_BT_EVT_CODEC_NEGOTIATED    5
#define DEMOD_BT_EVT_TRANSPORT_PENDING   6
#define DEMOD_BT_EVT_VOLUME_CHANGED      7
#define DEMOD_BT_EVT_ERROR              -1

typedef struct {
    int          event_type;   /* DEMOD_BT_EVT_* */
    int          fd;           /* BT transport fd (TRANSPORT_ACQUIRED) */
    unsigned int read_mtu;     /* (TRANSPORT_ACQUIRED) */
    unsigned int write_mtu;    /* (TRANSPORT_ACQUIRED) */
    char        *string_data;  /* free with demod_bt_free_string */
} demod_bt_event_t;

/* ── Lifecycle ────────────────────────────────────────────────── */

int  demod_bt_init(unsigned sample_rate, unsigned channels,
                   int direction, unsigned jitter_ms,
                   unsigned dcf_payload);
int  demod_bt_register(void);
int  demod_bt_acquire_and_start(const char *transport_path);
int  demod_bt_start_stream(int bt_fd, const uint8_t *codec_config,
                           unsigned codec_config_len);
void demod_bt_stop_stream(void);
int  demod_bt_is_streaming(void);
int  demod_bt_set_volume(unsigned volume);   /* 0-127, AVRCP scale */
int  demod_bt_set_volume_remote(unsigned volume); /* from remote AVRCP */
int  demod_bt_get_volume(void);              /* returns 0-127 */
int  demod_bt_update_metadata(const char *title,
                              const char *artist,
                              const char *album,
                              uint64_t duration_us);
int  demod_bt_update_playback_status(const char *status);
int  demod_bt_update_playback_position(uint64_t position_us);
void demod_bt_shutdown(void);

/* ── Event Polling ────────────────────────────────────────────── */

int  demod_bt_poll_event(demod_bt_event_t *out);

/* ── Metrics ──────────────────────────────────────────────────── */

int demod_bt_get_metrics(demod_bt_metrics_t *out);

/* ── DCF Constants ────────────────────────────────────────────── */

unsigned demod_bt_dcf_header_size(void);
unsigned demod_bt_dcf_optimal_payload(void);

/* ── Info ─────────────────────────────────────────────────────── */

const char *demod_bt_version(void);
char       *demod_bt_status(void);
void        demod_bt_free_string(char *s);

#ifdef __cplusplus
}
#endif

#endif /* DEMOD_BT_H */
