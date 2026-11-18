// Monocypher 4.0.2 is included unchanged, with its BSD/CC0 license.
// CPace uses 255-bit representatives; Monocypher's public Elligator API masks
// two bits for its *hiding* construction. This adapter uses the same internal
// constant-time arithmetic but masks only the top bit, per CPace draft-21.
#include "monocypher/monocypher.c"
#include "cpace_primitives.h"

void screx_cpace_map(uint8_t curve[32], const uint8_t representative[32])
{
    fe r, u, t1, t2, t3;
    fe_frombytes_mask(r, representative, 1);
    fe_sq(r, r);
    fe_add(t1, r, r);
    fe_add(u, t1, fe_one);
    fe_sq(t2, u);
    fe_mul(t3, A2, t1);
    fe_sub(t3, t3, t2);
    fe_mul(t3, t3, A);
    fe_mul(t1, t2, u);
    fe_mul(t1, t3, t1);
    int is_square = invsqrt(t1, t1);
    fe_mul(u, r, ufactor);
    fe_ccopy(u, fe_one, is_square);
    fe_sq(t1, t1);
    fe_mul(u, u, A);
    fe_mul(u, u, t3);
    fe_mul(u, u, t2);
    fe_mul(u, u, t1);
    fe_neg(u, u);
    fe_tobytes(curve, u);
    WIPE_BUFFER(t1); WIPE_BUFFER(r);
    WIPE_BUFFER(t2); WIPE_BUFFER(u);
    WIPE_BUFFER(t3);
}
