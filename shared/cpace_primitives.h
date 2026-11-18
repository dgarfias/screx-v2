#ifndef SCREX_CPACE_PRIMITIVES_H
#define SCREX_CPACE_PRIMITIVES_H
#include <stdint.h>
// CPace draft-21 X25519 Elligator2 mapping. Input is a 255-bit field encoding.
void screx_cpace_map(uint8_t curve[32], const uint8_t representative[32]);
#endif
