# picocell-gw

Low-cost LoRaWAN gateway: Seeed WM1302 (SPI) + W5500-EVB-Pico2, connecting directly to
ChirpStack over MQTT/TLS.

- [sketch.md](sketch.md): design
- [hw/](hw/): KiCad 8 schematic
- [fw/](fw/): firmware (Rust, Embassy), see [fw/README.md](fw/README.md)
- [tools/](tools/): test broker, MQTT/TLS host test, UF2 converter
