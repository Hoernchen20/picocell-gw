/*
 * Platform layer for sx1302_hal V2.1.0 on the RP2350 (sketch.md section 3.1). Replaces
 * the Linux files loragw_spi.c, sx1250_spi.c, sx125x_spi.c, sx1261_spi.c and
 * loragw_i2c.c with calls into Rust (pc.h), stubs the USB backends, and provides the
 * clock, stdio and syscalls newlib wants. The SPI framing is the one of the upstream
 * files; pc_start/pc_receive wrap the HAL calls lora_pkt_fwd makes.
 */

#include <errno.h>
#include <stdarg.h>
#include <string.h>
#include <sys/stat.h>

#include "loragw_aux.h"
#include "loragw_hal.h"
#include "loragw_i2c.h"
#include "loragw_spi.h"
#include "loragw_usb.h"
#include "sx1250_spi.h"
#include "sx1250_usb.h"
#include "sx125x_spi.h"
#include "sx1261_spi.h"
#include "sx1261_usb.h"

#include "pc.h"

_Static_assert(sizeof(struct pc_conf) == 160, "pc_conf layout");
_Static_assert(offsetof(struct pc_conf, clksrc) == 159, "pc_conf layout");
_Static_assert(sizeof(struct pc_rx) == 288, "pc_rx layout");
_Static_assert(offsetof(struct pc_rx, payload) == 32, "pc_rx layout");

#define READ_ACCESS 0x00
#define WRITE_ACCESS 0x80
#define LGW_BURST_CHUNK 1024
#define WAIT_BUSY_SX1250_MS 1

/* --- SPI (loragw_spi.c) --------------------------------------------------- */

/* The HAL only checks the target for NULL. */
static int spi_device;

static int xfer(const uint8_t *cmd, uint16_t cmd_len, uint8_t *data, uint16_t len, bool exchange) {
    return pc_spi(cmd, cmd_len, data, len, exchange) == 0 ? LGW_SPI_SUCCESS : LGW_SPI_ERROR;
}

int lgw_spi_open(const char *com_path, void **com_target_ptr) {
    (void)com_path;
    if (com_target_ptr == NULL) {
        return LGW_SPI_ERROR;
    }
    *com_target_ptr = &spi_device;
    return LGW_SPI_SUCCESS;
}

int lgw_spi_close(void *com_target) {
    (void)com_target;
    return LGW_SPI_SUCCESS;
}

int lgw_spi_w(void *com_target, uint8_t spi_mux_target, uint16_t address, uint8_t data) {
    const uint8_t cmd[3] = {spi_mux_target, WRITE_ACCESS | ((address >> 8) & 0x7F), address & 0xFF};
    (void)com_target;
    return xfer(cmd, sizeof cmd, &data, 1, false);
}

int lgw_spi_r(void *com_target, uint8_t spi_mux_target, uint16_t address, uint8_t *data) {
    const uint8_t cmd[4] = {spi_mux_target, READ_ACCESS | ((address >> 8) & 0x7F), address & 0xFF, 0x00};
    (void)com_target;
    if (data == NULL) {
        return LGW_SPI_ERROR;
    }
    *data = 0;
    return xfer(cmd, sizeof cmd, data, 1, true);
}

int lgw_spi_rmw(void *com_target, uint8_t spi_mux_target, uint16_t address, uint8_t offs, uint8_t leng,
                uint8_t data) {
    uint8_t old = 0;
    uint8_t mask = ((1 << leng) - 1) << offs;
    int stat = lgw_spi_r(com_target, spi_mux_target, address, &old);
    stat += lgw_spi_w(com_target, spi_mux_target, address, (~mask & old) | (mask & (uint8_t)(data << offs)));
    return stat;
}

/* Each chunk repeats the command: bursts go to data ports, not address ranges. */
int lgw_spi_wb(void *com_target, uint8_t spi_mux_target, uint16_t address, const uint8_t *data, uint16_t size) {
    const uint8_t cmd[3] = {spi_mux_target, WRITE_ACCESS | ((address >> 8) & 0x7F), address & 0xFF};
    (void)com_target;
    if (data == NULL || size == 0) {
        return LGW_SPI_ERROR;
    }
    for (uint16_t done = 0; done < size;) {
        uint16_t chunk = (size - done < LGW_BURST_CHUNK) ? size - done : LGW_BURST_CHUNK;
        /* Written only (exchange false), so the cast does not modify data. */
        if (xfer(cmd, sizeof cmd, (uint8_t *)data + done, chunk, false) != LGW_SPI_SUCCESS) {
            return LGW_SPI_ERROR;
        }
        done += chunk;
    }
    return LGW_SPI_SUCCESS;
}

int lgw_spi_rb(void *com_target, uint8_t spi_mux_target, uint16_t address, uint8_t *data, uint16_t size) {
    const uint8_t cmd[4] = {spi_mux_target, READ_ACCESS | ((address >> 8) & 0x7F), address & 0xFF, 0x00};
    (void)com_target;
    if (data == NULL || size == 0) {
        return LGW_SPI_ERROR;
    }
    /* spidev clocks out zeros for a read-only transfer. */
    memset(data, 0, size);
    for (uint16_t done = 0; done < size;) {
        uint16_t chunk = (size - done < LGW_BURST_CHUNK) ? size - done : LGW_BURST_CHUNK;
        if (xfer(cmd, sizeof cmd, data + done, chunk, true) != LGW_SPI_SUCCESS) {
            return LGW_SPI_ERROR;
        }
        done += chunk;
    }
    return LGW_SPI_SUCCESS;
}

uint16_t lgw_spi_chunk_size(void) {
    return LGW_BURST_CHUNK;
}

/* --- SX1250 and SX125x behind the SX1302's SPI mux ------------------------ */

int sx1250_spi_w(void *com_target, uint8_t spi_mux_target, sx1250_op_code_t op_code, uint8_t *data, uint16_t size) {
    const uint8_t cmd[2] = {spi_mux_target, (uint8_t)op_code};
    (void)com_target;
    wait_ms(WAIT_BUSY_SX1250_MS);
    if (data == NULL) {
        return LGW_SPI_ERROR;
    }
    return xfer(cmd, sizeof cmd, data, size, false);
}

int sx1250_spi_r(void *com_target, uint8_t spi_mux_target, sx1250_op_code_t op_code, uint8_t *data, uint16_t size) {
    const uint8_t cmd[2] = {spi_mux_target, (uint8_t)op_code};
    (void)com_target;
    wait_ms(WAIT_BUSY_SX1250_MS);
    if (data == NULL) {
        return LGW_SPI_ERROR;
    }
    return xfer(cmd, sizeof cmd, data, size, true);
}

int sx125x_spi_w(void *com_target, uint8_t spi_mux_target, uint8_t address, uint8_t data) {
    const uint8_t cmd[2] = {spi_mux_target, WRITE_ACCESS | (address & 0x7F)};
    (void)com_target;
    return xfer(cmd, sizeof cmd, &data, 1, false);
}

int sx125x_spi_r(void *com_target, uint8_t spi_mux_target, uint8_t address, uint8_t *data) {
    const uint8_t cmd[2] = {spi_mux_target, READ_ACCESS | (address & 0x7F)};
    (void)com_target;
    if (data == NULL) {
        return LGW_SPI_ERROR;
    }
    *data = 0;
    return xfer(cmd, sizeof cmd, data, 1, true);
}

/* No SX1261 on this WM1302 variant (LBT, spectral scan). */
int sx1261_spi_w(void *com_target, sx1261_op_code_t op_code, uint8_t *data, uint16_t size) {
    (void)com_target, (void)op_code, (void)data, (void)size;
    return LGW_SPI_ERROR;
}

int sx1261_spi_r(void *com_target, sx1261_op_code_t op_code, uint8_t *data, uint16_t size) {
    (void)com_target, (void)op_code, (void)data, (void)size;
    return LGW_SPI_ERROR;
}

/* --- USB backends (not used) ---------------------------------------------- */

int lgw_usb_open(const char *com_path, void **com_target_ptr) {
    (void)com_path, (void)com_target_ptr;
    return LGW_USB_ERROR;
}

int lgw_usb_close(void *com_target) {
    (void)com_target;
    return LGW_USB_ERROR;
}

int lgw_usb_w(void *com_target, uint8_t spi_mux_target, uint16_t address, uint8_t data) {
    (void)com_target, (void)spi_mux_target, (void)address, (void)data;
    return LGW_USB_ERROR;
}

int lgw_usb_r(void *com_target, uint8_t spi_mux_target, uint16_t address, uint8_t *data) {
    (void)com_target, (void)spi_mux_target, (void)address, (void)data;
    return LGW_USB_ERROR;
}

int lgw_usb_wb(void *com_target, uint8_t spi_mux_target, uint16_t address, const uint8_t *data, uint16_t size) {
    (void)com_target, (void)spi_mux_target, (void)address, (void)data, (void)size;
    return LGW_USB_ERROR;
}

int lgw_usb_rb(void *com_target, uint8_t spi_mux_target, uint16_t address, uint8_t *data, uint16_t size) {
    (void)com_target, (void)spi_mux_target, (void)address, (void)data, (void)size;
    return LGW_USB_ERROR;
}

int lgw_usb_rmw(void *com_target, uint16_t address, uint8_t offs, uint8_t leng, uint8_t data) {
    (void)com_target, (void)address, (void)offs, (void)leng, (void)data;
    return LGW_USB_ERROR;
}

int lgw_usb_set_write_mode(lgw_com_write_mode_t write_mode) {
    (void)write_mode;
    return LGW_USB_ERROR;
}

int lgw_usb_flush(void *com_target) {
    (void)com_target;
    return LGW_USB_ERROR;
}

uint16_t lgw_usb_chunk_size(void) {
    return 0;
}

int lgw_usb_get_temperature(void *com_target, float *temperature) {
    (void)com_target, (void)temperature;
    return LGW_USB_ERROR;
}

int sx1250_usb_w(void *com_target, uint8_t spi_mux_target, sx1250_op_code_t op_code, uint8_t *data, uint16_t size) {
    (void)com_target, (void)spi_mux_target, (void)op_code, (void)data, (void)size;
    return LGW_USB_ERROR;
}

int sx1250_usb_r(void *com_target, uint8_t spi_mux_target, sx1250_op_code_t op_code, uint8_t *data, uint16_t size) {
    (void)com_target, (void)spi_mux_target, (void)op_code, (void)data, (void)size;
    return LGW_USB_ERROR;
}

int sx1261_usb_w(void *com_target, sx1261_op_code_t op_code, uint8_t *data, uint16_t size) {
    (void)com_target, (void)op_code, (void)data, (void)size;
    return LGW_USB_ERROR;
}

int sx1261_usb_r(void *com_target, sx1261_op_code_t op_code, uint8_t *data, uint16_t size) {
    (void)com_target, (void)op_code, (void)data, (void)size;
    return LGW_USB_ERROR;
}

int sx1261_usb_set_write_mode(lgw_com_write_mode_t write_mode) {
    (void)write_mode;
    return LGW_USB_ERROR;
}

int sx1261_usb_flush(void *com_target) {
    (void)com_target;
    return LGW_USB_ERROR;
}

/* --- I2C (loragw_i2c.c): the temperature sensor --------------------------- */

int i2c_linuxdev_open(const char *path, uint8_t device_addr, int *i2c_fd) {
    (void)path;
    if (i2c_fd == NULL) {
        return LGW_I2C_ERROR;
    }
    *i2c_fd = device_addr;
    return LGW_I2C_SUCCESS;
}

int i2c_linuxdev_close(int i2c_fd) {
    (void)i2c_fd;
    return LGW_I2C_SUCCESS;
}

int i2c_linuxdev_read(int i2c_fd, uint8_t device_addr, uint8_t reg_addr, uint8_t *data) {
    (void)i2c_fd;
    if (data == NULL) {
        return LGW_I2C_ERROR;
    }
    return pc_i2c(device_addr, &reg_addr, 1, data, 1) == 0 ? LGW_I2C_SUCCESS : LGW_I2C_ERROR;
}

int i2c_linuxdev_write(int i2c_fd, uint8_t device_addr, uint8_t reg_addr, uint8_t data) {
    const uint8_t buf[2] = {reg_addr, data};
    (void)i2c_fd;
    return pc_i2c(device_addr, buf, sizeof buf, NULL, 0) == 0 ? LGW_I2C_SUCCESS : LGW_I2C_ERROR;
}

int i2c_linuxdev_write_buffer(int i2c_fd, uint8_t device_addr, uint8_t *buffer, uint8_t size) {
    (void)i2c_fd;
    if (buffer == NULL) {
        return LGW_I2C_ERROR;
    }
    return pc_i2c(device_addr, buffer, size, NULL, 0) == 0 ? LGW_I2C_SUCCESS : LGW_I2C_ERROR;
}

/* --- Clock (wait_ms/wait_us, timeouts in loragw_aux.c) -------------------- */

int clock_nanosleep(clockid_t clock_id, int flags, const struct timespec *request, struct timespec *remain) {
    (void)clock_id, (void)flags;
    uint64_t us = (uint64_t)request->tv_sec * 1000000u + (uint64_t)request->tv_nsec / 1000u;
    while (us > 0) {
        uint32_t step = us > UINT32_MAX ? UINT32_MAX : (uint32_t)us;
        pc_delay_us(step);
        us -= step;
    }
    if (remain != NULL) {
        remain->tv_sec = 0;
        remain->tv_nsec = 0;
    }
    return 0;
}

/* gettimeofday() and time(): time since boot. */
int _gettimeofday(struct timeval *tv, void *tz) {
    (void)tz;
    uint64_t now = pc_now_us();
    tv->tv_sec = (time_t)(now / 1000000u);
    tv->tv_usec = (suseconds_t)(now % 1000000u);
    return 0;
}

/* --- printf to the log, one call per line --------------------------------- */

static char line[160];
static size_t line_len;

static void flush_line(void) {
    int level = 3;
    if (strncmp(line, "ERROR", 5) == 0) {
        level = 1;
    } else if (strncmp(line, "WARNING", 7) == 0) {
        level = 2;
    }
    if (line_len > 0) {
        pc_log(level, line, line_len);
    }
    line_len = 0;
}

static int pc_vprintf(const char *fmt, va_list ap) {
    char buf[160];
    int n = vsnprintf(buf, sizeof buf, fmt, ap);
    if (n < 0) {
        return n;
    }
    size_t len = (size_t)n < sizeof buf ? (size_t)n : sizeof buf - 1;
    for (size_t i = 0; i < len; i++) {
        if (buf[i] == '\n') {
            flush_line();
        } else {
            if (line_len == sizeof line) {
                flush_line();
            }
            line[line_len++] = buf[i];
        }
    }
    return n;
}

int pc_printf(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int n = pc_vprintf(fmt, ap);
    va_end(ap);
    return n;
}

int pc_fprintf(FILE *stream, const char *fmt, ...) {
    (void)stream;
    va_list ap;
    va_start(ap, fmt);
    int n = pc_vprintf(fmt, ap);
    va_end(ap);
    return n;
}

/* --- newlib syscalls: no files, no heap ----------------------------------- */

void *_sbrk(ptrdiff_t incr) {
    (void)incr;
    errno = ENOMEM;
    return (void *)-1;
}

int _write(int fd, const char *buf, int len) {
    (void)fd, (void)buf;
    return len;
}

int _read(int fd, char *buf, int len) {
    (void)fd, (void)buf, (void)len;
    return -1;
}

int _close(int fd) {
    (void)fd;
    return -1;
}

int _fstat(int fd, struct stat *st) {
    (void)fd;
    st->st_mode = S_IFCHR;
    return 0;
}

int _isatty(int fd) {
    (void)fd;
    return 1;
}

int _lseek(int fd, int offset, int whence) {
    (void)fd, (void)offset, (void)whence;
    return 0;
}

int _kill(int pid, int sig) {
    (void)pid, (void)sig;
    errno = EINVAL;
    return -1;
}

int _getpid(void) {
    return 1;
}

void _exit(int status) {
    (void)status;
    pc_abort();
}

/* --- Concentrator ---------------------------------------------------------- */

static uint8_t bandwidth_code(uint32_t hz) {
    switch (hz) {
    case 125000:
        return BW_125KHZ;
    case 250000:
        return BW_250KHZ;
    case 500000:
        return BW_500KHZ;
    default:
        return BW_UNDEFINED;
    }
}

static uint32_t bandwidth_hz(uint8_t code) {
    switch (code) {
    case BW_125KHZ:
        return 125000;
    case BW_250KHZ:
        return 250000;
    case BW_500KHZ:
        return 500000;
    default:
        return 0;
    }
}

/* lora_pkt_fwd's order: board, RF chains, demodulators, IF chains, start. */
int pc_start(const struct pc_conf *c, uint64_t *eui) {
    struct lgw_conf_board_s board;
    memset(&board, 0, sizeof board);
    board.lorawan_public = c->lorawan_public;
    board.clksrc = c->clksrc;
    board.full_duplex = false;
    board.com_type = LGW_COM_SPI;
    strcpy(board.com_path, "spi1");
    if (lgw_board_setconf(&board) != LGW_HAL_SUCCESS) {
        return PC_ERR_BOARD;
    }

    for (uint8_t i = 0; i < 2; i++) {
        const struct pc_radio *r = &c->radio[i];
        struct lgw_conf_rxrf_s rf;
        memset(&rf, 0, sizeof rf);
        rf.enable = r->enable;
        rf.freq_hz = r->freq_hz;
        rf.rssi_offset = r->rssi_offset;
        rf.rssi_tcomp.coeff_a = r->rssi_tcomp[0];
        rf.rssi_tcomp.coeff_b = r->rssi_tcomp[1];
        rf.rssi_tcomp.coeff_c = r->rssi_tcomp[2];
        rf.rssi_tcomp.coeff_d = r->rssi_tcomp[3];
        rf.rssi_tcomp.coeff_e = r->rssi_tcomp[4];
        rf.type = LGW_RADIO_TYPE_SX1250;
        rf.tx_enable = r->tx_enable;
        rf.single_input_mode = false;
        if (lgw_rxrf_setconf(i, &rf) != LGW_HAL_SUCCESS) {
            return PC_ERR_RXRF;
        }
    }

    struct lgw_conf_demod_s demod = {.multisf_datarate = c->multi_sf_mask};
    if (lgw_demod_setconf(&demod) != LGW_HAL_SUCCESS) {
        return PC_ERR_DEMOD;
    }

    struct lgw_conf_rxif_s ifc;
    for (uint8_t i = 0; i < 8; i++) {
        memset(&ifc, 0, sizeof ifc);
        ifc.enable = c->multi_sf[i].enable;
        ifc.rf_chain = c->multi_sf[i].radio;
        ifc.freq_hz = c->multi_sf[i].if_hz;
        if (lgw_rxif_setconf(i, &ifc) != LGW_HAL_SUCCESS) {
            return PC_ERR_RXIF;
        }
    }
    memset(&ifc, 0, sizeof ifc);
    ifc.enable = c->lora_std.enable;
    ifc.rf_chain = c->lora_std.radio;
    ifc.freq_hz = c->lora_std.if_hz;
    ifc.bandwidth = bandwidth_code(c->lora_std_bandwidth_hz);
    ifc.datarate = c->lora_std_sf;
    if (lgw_rxif_setconf(8, &ifc) != LGW_HAL_SUCCESS) {
        return PC_ERR_RXIF;
    }
    memset(&ifc, 0, sizeof ifc);
    ifc.enable = c->fsk.enable;
    ifc.rf_chain = c->fsk.radio;
    ifc.freq_hz = c->fsk.if_hz;
    ifc.bandwidth = bandwidth_code(c->fsk_bandwidth_hz);
    ifc.datarate = c->fsk_datarate;
    if (lgw_rxif_setconf(9, &ifc) != LGW_HAL_SUCCESS) {
        return PC_ERR_RXIF;
    }

    if (lgw_start() != LGW_HAL_SUCCESS) {
        return PC_ERR_START;
    }
    if (lgw_get_eui(eui) != LGW_HAL_SUCCESS) {
        return PC_ERR_EUI;
    }
    return 0;
}

int pc_stop(void) {
    return lgw_stop() == LGW_HAL_SUCCESS ? 0 : -1;
}

static struct lgw_pkt_rx_s rx[PC_RX_MAX];

int pc_receive(struct pc_rx *out, uint8_t max) {
    if (max > PC_RX_MAX) {
        max = PC_RX_MAX;
    }
    int n = lgw_receive(max, rx);
    if (n < 0) {
        return -1;
    }
    for (int i = 0; i < n; i++) {
        const struct lgw_pkt_rx_s *p = &rx[i];
        struct pc_rx *o = &out[i];
        memset(o, 0, offsetof(struct pc_rx, payload));
        o->freq_hz = p->freq_hz;
        o->count_us = p->count_us;
        o->bandwidth_hz = bandwidth_hz(p->bandwidth);
        o->datarate = p->datarate;
        o->rssi = p->rssic;
        o->snr = p->snr;
        o->size = p->size;
        o->if_chain = p->if_chain;
        o->rf_chain = p->rf_chain;
        o->modulation = p->modulation == MOD_FSK ? PC_MOD_FSK : PC_MOD_LORA;
        switch (p->status) {
        case STAT_NO_CRC:
            o->crc = PC_CRC_NONE;
            break;
        case STAT_CRC_OK:
            o->crc = PC_CRC_OK;
            break;
        case STAT_CRC_BAD:
            o->crc = PC_CRC_BAD;
            break;
        default:
            o->crc = PC_CRC_UNKNOWN;
            break;
        }
        switch (p->coderate) {
        case CR_LORA_4_5:
            o->coderate = 5;
            break;
        case CR_LORA_4_6:
            o->coderate = 6;
            break;
        case CR_LORA_4_7:
            o->coderate = 7;
            break;
        case CR_LORA_4_8:
            o->coderate = 8;
            break;
        default:
            o->coderate = 0;
            break;
        }
        memcpy(o->payload, p->payload, p->size <= sizeof o->payload ? p->size : sizeof o->payload);
    }
    return n;
}
