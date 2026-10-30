/*
 * Interface between the Rust crate (src/lib.rs) and the HAL: flat structs of fixed-size
 * types only, mirrored with #[repr(C)] there. Sizes and the offset of the last field are
 * checked on both sides.
 */
#ifndef PC_H
#define PC_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

/* --- implemented in Rust; 0 on success ------------------------------------ */

/* One transaction with CS low: cmd, then data (written, or exchanged in place). */
int pc_spi(const uint8_t *cmd, uint16_t cmd_len, uint8_t *data, uint16_t len, bool exchange);
/* Writes wr, then reads rd after a repeated start (rd_len 0: write only). */
int pc_i2c(uint8_t addr, const uint8_t *wr, uint16_t wr_len, uint8_t *rd, uint16_t rd_len);
void pc_delay_us(uint32_t us);
uint64_t pc_now_us(void);
/* One line of HAL output without the newline; level 1 error, 2 warning, 3 info. */
void pc_log(int level, const char *msg, size_t len);
void pc_abort(void) __attribute__((noreturn));

/* --- implemented in shim.c ------------------------------------------------ */

struct pc_radio {
    uint32_t freq_hz;
    float rssi_offset;
    float rssi_tcomp[5]; /* coeff_a..coeff_e */
    uint8_t enable;
    uint8_t tx_enable;
    uint8_t pad[2];
};

struct pc_chan {
    int32_t if_hz; /* relative to the radio's frequency */
    uint8_t enable;
    uint8_t radio;
    uint8_t pad[2];
};

struct pc_conf {
    struct pc_radio radio[2];
    struct pc_chan multi_sf[8];
    struct pc_chan lora_std;
    struct pc_chan fsk;
    uint32_t lora_std_bandwidth_hz;
    uint32_t fsk_bandwidth_hz;
    uint32_t fsk_datarate;
    uint8_t lora_std_sf;
    uint8_t multi_sf_mask; /* bit n: SF5 + n */
    uint8_t lorawan_public;
    uint8_t clksrc; /* radio that clocks the SX1302 */
};

#define PC_MOD_LORA 0
#define PC_MOD_FSK 1

#define PC_CRC_NONE 0
#define PC_CRC_OK 1
#define PC_CRC_BAD 2
#define PC_CRC_UNKNOWN 3

struct pc_rx {
    uint32_t freq_hz;
    uint32_t count_us;
    uint32_t bandwidth_hz;
    uint32_t datarate; /* LoRa: spreading factor, FSK: bit/s */
    float rssi;        /* channel RSSI, dBm */
    float snr;         /* LoRa, dB */
    uint16_t size;
    uint8_t if_chain;
    uint8_t rf_chain;
    uint8_t modulation; /* PC_MOD_* */
    uint8_t crc;        /* PC_CRC_* */
    uint8_t coderate;   /* LoRa 4/5..4/8 as 5..8 */
    uint8_t pad;
    uint8_t payload[256];
};

/* Negative step on failure: PC_ERR_* (the HAL logs the details). */
#define PC_ERR_BOARD -1
#define PC_ERR_RXRF -2
#define PC_ERR_DEMOD -3
#define PC_ERR_RXIF -4
#define PC_ERR_START -5
#define PC_ERR_EUI -6

int pc_start(const struct pc_conf *conf, uint64_t *eui);
int pc_stop(void);
/* Number of frames written to out (at most PC_RX_MAX), or -1. */
#define PC_RX_MAX 8
int pc_receive(struct pc_rx *out, uint8_t max);

#endif
