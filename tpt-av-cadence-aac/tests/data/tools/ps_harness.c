/* Standalone oracle for FFmpeg's float Parametric Stereo synthesis
 * (ff_ps_apply). Feeds deterministic synthetic QMF-domain input through
 * the reference implementation and dumps the last frame's L/R output as
 * raw f32, plus every generated table, so a port can be compared
 * value-for-value. Build:
 *   gcc -O2 -ffp-contract=off -I. -Ilibavutil ps_harness.c aacps.c \
 *       aacpsdata.c aacpsdsp_float.c -o ps_oracle
 */
#include <stdio.h>
#include <stdlib.h>
#include "aacps.h"
#include "aacpsdsp.h"
#include "aacps.c" /* single-TU harness: tablegen statics + ff_ps_apply */

static uint32_t lcg_state;

static float lcg_f(void)
{
    lcg_state = lcg_state * 1664525u + 1013904223u;
    return (((lcg_state >> 8) & 0xFFFFFF) / 16777215.0f) * 2.0f - 1.0f;
}

/* Tables are static inside aacps_tablegen.h, so the dump lives in aacps.c. */

static void fill_frame(float L[2][38][64], uint32_t seed)
{
    int i, k;
    lcg_state = seed;
    for (i = 0; i < 38; i++)
        for (k = 0; k < 64; k++) {
            L[0][i][k] = lcg_f();
            L[1][i][k] = lcg_f();
        }
}

static const char *outdir;

static FILE *stage_dump;
static void run_case(const char *name, PSContext *ps, int top, uint32_t seed,
                     int frames, int is34_last)
{
    static float L[2][38][64], R[2][38][64];
    char path[512];
    FILE *f;
    int i;

    for (i = 0; i < frames; i++) {
        fill_frame(L, seed + (uint32_t)i * 7919u);
        /* A mid-stream band-mode switch models a real 20->34 reconfig:
         * common.is34bands must already carry the new mode when
         * ff_ps_apply runs (the parser sets it at end of frame). */
        if (is34_last >= 0)
            ps->common.is34bands = (i < is34_last) ? 0 : 1;
        ff_ps_apply(ps, L, R, top);
    }
    snprintf(path, sizeof(path), "%s/%s", outdir, name);
    f = fopen(path, "wb");
    if (!f) { perror(path); exit(1); }
    fwrite(L, sizeof(float), 2 * 38 * 64, f);
    fwrite(R, sizeof(float), 2 * 38 * 64, f);
    fclose(f);
}

static void set_params(PSContext *ps, int iid_quant, int nr_iid, int icc_mode,
                       int nr_icc, int nr_ipdopd, int enable_ipdopd,
                       int num_env, const int *borders)
{
    int e, b;
    memset(ps, 0, sizeof(*ps));
    AAC_RENAME(ff_psdsp_init)(&ps->dsp); // memset wipes the dsp function pointers
    ps->common.start = 1;
    ps->common.enable_iid = 1;
    ps->common.iid_quant = iid_quant;
    ps->common.nr_iid_par = nr_iid;
    ps->common.enable_icc = 1;
    ps->common.icc_mode = icc_mode;
    ps->common.nr_icc_par = nr_icc;
    ps->common.enable_ext = enable_ipdopd;
    ps->common.enable_ipdopd = enable_ipdopd;
    ps->common.nr_ipdopd_par = nr_ipdopd;
    ps->common.num_env = num_env;
    ps->common.border_position[0] = -1;
    for (e = 1; e <= num_env; e++)
        ps->common.border_position[e] = borders[e - 1];
    for (e = 0; e < num_env; e++) {
        for (b = 0; b < nr_iid; b++)
            ps->common.iid_par[e][b] = (int8_t)(((7 * b + 5 * e) % 15) - 7);
        for (b = 0; b < nr_icc; b++)
            ps->common.icc_par[e][b] = (int8_t)((b + e) % 8);
        for (b = 0; b < nr_ipdopd; b++) {
            ps->common.ipd_par[e][b] = (int8_t)((3 * b + e) % 8);
            ps->common.opd_par[e][b] = (int8_t)((5 * b + 2 * e) % 8);
        }
    }
}

int main(int argc, char **argv)
{
    PSContext ps;
    static const int borders_a[2] = { 16, 31 };
    static const int borders_b[1] = { 31 };
    static const int borders_c[4] = { 7, 15, 23, 31 };

    if (argc < 2) { fprintf(stderr, "usage: %s <outdir>\n", argv[0]); return 1; }
    outdir = argv[1];
    ps_tableinit();
    dump_tables(argv[1]);

    /* A: 20-band fine quant, ipd/opd on, HB mixing (icc_mode >= 3),
     * two envelopes. top = kx[1] + m[1] at full band. */
    set_params(&ps, 1, 20, 3, 20, 11, 1, 2, borders_a);
    ps.common.is34bands = 0;
    ps.common.is34bands_old = 0;
    run_case("a_20band_ipd_last.f32", &ps, 64, 12345, 8, -1);

    /* B: 34-band, ipd/opd on, one envelope. */
    set_params(&ps, 1, 34, 5, 34, 17, 1, 1, borders_b);
    ps.common.is34bands = 1;
    ps.common.is34bands_old = 1;
    run_case("b_34band_ipd_last.f32", &ps, 52, 98765, 8, -1);

    /* C: 10-band coarse params remapped to 20, ipd/opd off (HA mixing,
     * flat stereo_interpolate), four envelopes. */
    set_params(&ps, 0, 10, 1, 10, 5, 0, 4, borders_c);
    ps.common.is34bands = 0;
    ps.common.is34bands_old = 0;
    run_case("c_10band_baseline_last.f32", &ps, 64, 55555, 8, -1);

    /* D: live band-mode switch 20 -> 34 after three frames; exercises
     * the is34bands_old reset paths in decorrelation and stereo
     * processing (H history remap + ipd/opd histogram reset). */
    set_params(&ps, 1, 20, 3, 20, 11, 1, 2, borders_a);
    ps.common.is34bands = 0;
    ps.common.is34bands_old = 0;
    run_case("d_modeswitch_last.f32", &ps, 48, 424242, 8, 3);

    return 0;
}
