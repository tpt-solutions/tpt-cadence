/* Standalone oracle for FFmpeg's ff_ps_read_data parameter parsing.
 * Decodes PS payload bit-strings (one per line, '0'/'1' characters)
 * through a copy of aacps_common.c whose VLC lookups are replaced by an
 * equivalent canonical decoder over aacps_huff_tabs, then prints the
 * resulting PSCommonContext. Build:
 *   gcc -O2 -I. -Ilibavutil ps_read_oracle.c aacpsdata.c -o ps_read_oracle
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

/* ---- minimal GetBitContext ---- */
typedef struct GetBitContext {
    const uint8_t *buffer;
    int size_in_bits;
    int index;
} GetBitContext;

static int init_get_bits8(GetBitContext *s, const uint8_t *buffer, int byte_size)
{
    s->buffer = buffer;
    s->size_in_bits = byte_size * 8;
    s->index = 0;
    return 0;
}
static inline int get_bits_count(const GetBitContext *s) { return s->index; }
static inline int get_bit_at(GetBitContext *s, int idx)
{
    if (idx >= s->size_in_bits) return 0;
    return (s->buffer[idx >> 3] >> (7 - (idx & 7))) & 1;
}
static inline unsigned int get_bits(GetBitContext *s, int n)
{
    unsigned int v = 0;
    for (int i = 0; i < n; i++) v = (v << 1) | get_bit_at(s, s->index++);
    return v;
}
static inline unsigned int get_bits1(GetBitContext *s) { return get_bit_at(s, s->index++); }
static inline unsigned int show_bits(GetBitContext *s, int n)
{
    unsigned int v = 0;
    for (int i = 0; i < n; i++) v = (v << 1) | get_bit_at(s, s->index + i);
    return v;
}
static inline void skip_bits(GetBitContext *s, int n) { s->index += n; }
static inline void skip_bits_long(GetBitContext *s, int n) { s->index += n; }

/* ---- VLC replacement: canonical codes over aacps_huff_tabs ---- */
static const uint8_t ps_huff_tabs_local[][2] = {
    { 28, 4 },
    { 32, 4 },
    { 29, 3 },
    { 31, 3 },
    { 27, 5 },
    { 33, 5 },
    { 26, 6 },
    { 34, 6 },
    { 25, 7 },
    { 35, 7 },
    { 24, 8 },
    { 36, 8 },
    { 37, 9 },
    { 40, 11 },
    { 19, 12 },
    { 41, 12 },
    { 22, 10 },
    { 38, 10 },
    { 9, 17 },
    { 51, 17 },
    { 11, 17 },
    { 49, 17 },
    { 13, 16 },
    { 47, 16 },
    { 16, 14 },
    { 18, 13 },
    { 42, 13 },
    { 44, 14 },
    { 12, 17 },
    { 48, 17 },
    { 4, 18 },
    { 5, 18 },
    { 2, 18 },
    { 3, 18 },
    { 15, 15 },
    { 21, 11 },
    { 39, 11 },
    { 45, 15 },
    { 8, 18 },
    { 52, 18 },
    { 6, 18 },
    { 7, 18 },
    { 55, 18 },
    { 56, 18 },
    { 53, 18 },
    { 54, 18 },
    { 17, 14 },
    { 43, 14 },
    { 59, 18 },
    { 60, 18 },
    { 57, 18 },
    { 58, 18 },
    { 0, 18 },
    { 1, 18 },
    { 10, 18 },
    { 50, 18 },
    { 14, 16 },
    { 46, 16 },
    { 20, 12 },
    { 23, 10 },
    { 30, 1 },
    { 31, 2 },
    { 26, 7 },
    { 34, 7 },
    { 27, 6 },
    { 33, 6 },
    { 35, 8 },
    { 24, 9 },
    { 36, 9 },
    { 39, 11 },
    { 41, 12 },
    { 9, 15 },
    { 10, 15 },
    { 48, 15 },
    { 49, 15 },
    { 17, 13 },
    { 23, 10 },
    { 37, 10 },
    { 43, 13 },
    { 11, 15 },
    { 12, 15 },
    { 4, 16 },
    { 56, 16 },
    { 2, 16 },
    { 3, 16 },
    { 59, 16 },
    { 60, 16 },
    { 57, 16 },
    { 58, 16 },
    { 0, 16 },
    { 1, 16 },
    { 5, 16 },
    { 55, 16 },
    { 6, 16 },
    { 54, 16 },
    { 13, 15 },
    { 15, 14 },
    { 20, 12 },
    { 40, 12 },
    { 22, 11 },
    { 38, 11 },
    { 45, 14 },
    { 47, 15 },
    { 7, 16 },
    { 53, 16 },
    { 18, 13 },
    { 42, 13 },
    { 16, 14 },
    { 44, 14 },
    { 8, 16 },
    { 52, 16 },
    { 14, 15 },
    { 46, 15 },
    { 50, 16 },
    { 51, 16 },
    { 19, 13 },
    { 21, 12 },
    { 25, 9 },
    { 28, 5 },
    { 32, 5 },
    { 29, 3 },
    { 30, 1 },
    { 14, 1 },
    { 15, 3 },
    { 13, 3 },
    { 16, 4 },
    { 12, 4 },
    { 17, 5 },
    { 11, 5 },
    { 10, 6 },
    { 18, 6 },
    { 19, 6 },
    { 9, 7 },
    { 20, 8 },
    { 8, 9 },
    { 7, 10 },
    { 21, 11 },
    { 22, 13 },
    { 6, 13 },
    { 23, 14 },
    { 24, 14 },
    { 5, 15 },
    { 25, 15 },
    { 4, 16 },
    { 3, 17 },
    { 0, 17 },
    { 1, 17 },
    { 2, 17 },
    { 26, 17 },
    { 27, 18 },
    { 28, 18 },
    { 14, 1 },
    { 13, 2 },
    { 15, 3 },
    { 12, 4 },
    { 16, 5 },
    { 11, 6 },
    { 17, 7 },
    { 10, 8 },
    { 18, 9 },
    { 9, 10 },
    { 19, 11 },
    { 8, 12 },
    { 20, 13 },
    { 21, 14 },
    { 7, 15 },
    { 22, 17 },
    { 6, 17 },
    { 23, 19 },
    { 0, 19 },
    { 1, 19 },
    { 2, 19 },
    { 3, 20 },
    { 4, 20 },
    { 5, 20 },
    { 24, 20 },
    { 25, 20 },
    { 26, 20 },
    { 27, 20 },
    { 28, 20 },
    { 7, 1 },
    { 8, 2 },
    { 6, 3 },
    { 9, 4 },
    { 5, 5 },
    { 10, 6 },
    { 4, 7 },
    { 11, 8 },
    { 12, 9 },
    { 3, 10 },
    { 13, 11 },
    { 2, 12 },
    { 14, 13 },
    { 1, 14 },
    { 0, 14 },
    { 7, 1 },
    { 8, 2 },
    { 6, 3 },
    { 9, 4 },
    { 5, 5 },
    { 10, 6 },
    { 4, 7 },
    { 11, 8 },
    { 3, 9 },
    { 12, 10 },
    { 2, 11 },
    { 13, 12 },
    { 1, 13 },
    { 0, 14 },
    { 14, 14 },
    { 1, 3 },
    { 4, 4 },
    { 5, 4 },
    { 3, 4 },
    { 6, 4 },
    { 2, 4 },
    { 7, 4 },
    { 0, 1 },
    { 5, 4 },
    { 4, 5 },
    { 3, 5 },
    { 2, 4 },
    { 6, 4 },
    { 1, 3 },
    { 7, 3 },
    { 0, 1 },
    { 7, 3 },
    { 1, 3 },
    { 3, 4 },
    { 6, 4 },
    { 2, 4 },
    { 5, 5 },
    { 4, 5 },
    { 0, 1 },
    { 5, 4 },
    { 2, 4 },
    { 6, 4 },
    { 4, 5 },
    { 3, 5 },
    { 1, 3 },
    { 7, 3 },
    { 0, 1 },
};

static const int huff_sizes_internal[] = { 61, 61, 29, 29, 15, 15, 8, 8, 8, 8 };
static const int huff_offset_internal[] = { -30, -30, -14, -14, -7, -7, 0, 0, 0, 0 };

typedef struct MyVLC {
    int nb_codes;
    uint32_t *codes;
    uint8_t *bits;
    int8_t *syms;
} MyVLC;

static MyVLC vlc_ps_internal[10];

static void build_canonical(MyVLC *v, int tab)
{
    int start = 0;
    for (int i = 0; i < tab; i++) start += huff_sizes_internal[i];
    int n = huff_sizes_internal[tab];
    v->nb_codes = n;
    v->codes = malloc(n * sizeof(uint32_t));
    v->bits = malloc(n);
    v->syms = malloc(n);
    uint32_t running = 0;
    int j = 0;
    for (int i = 0; i < n; i++) {
        uint8_t sym = ps_huff_tabs_local[start + i][0];
        uint8_t len = ps_huff_tabs_local[start + i][1];
        if (len == 0) continue;
        v->codes[j] = running >> (32 - len);
        v->bits[j] = len;
        v->syms[j] = (int8_t)((int)sym + huff_offset_internal[tab]);
        running += 1u << (32 - len);
        j++;
    }
    v->nb_codes = j;
}

static int trace_on = 0;
static int trace_pos_base = 0;
static int trace_band_base = 0;

static int my_get_vlc2(GetBitContext *gb, int table_idx)
{
    MyVLC *v = &vlc_ps_internal[table_idx];
    int start_index = gb->index;
    uint32_t acc = 0;
    for (int depth = 1; depth <= 17; depth++) {
        acc = (acc << 1) | get_bits1(gb);
        for (int i = 0; i < v->nb_codes; i++) {
            if (v->bits[i] == depth && v->codes[i] == acc) {
                if (trace_on)
                    fprintf(stderr, "CSTEP table=%d pos=%d bits=%d sym=%d\n",
                            table_idx, start_index - trace_pos_base, depth, v->syms[i]);
                return v->syms[i];
            }
        }
    }
    return 0;
}

#define FFMAX(a,b) ((a) > (b) ? (a) : (b))
#define FFABS(a) ((a) >= 0 ? (a) : (-(a)))

#define PS_MAX_NUM_ENV 5
#define PS_MAX_NR_IIDICC 34
#define PS_MAX_NR_IPDOPD 17
#define PS_BASELINE 0
#define numQMFSlots 32
static inline void skip_bits1(GetBitContext *s) { s->index += 1; }

typedef struct PSCommonContext {
    int    start;
    int    enable_iid;
    int    iid_quant;
    int    nr_iid_par;
    int    nr_ipdopd_par;
    int    enable_icc;
    int    icc_mode;
    int    nr_icc_par;
    int    enable_ext;
    int    frame_class;
    int    num_env_old;
    int    num_env;
    int    enable_ipdopd;
    int    border_position[PS_MAX_NUM_ENV + 1];
    int8_t iid_par[PS_MAX_NUM_ENV][PS_MAX_NR_IIDICC];
    int8_t icc_par[PS_MAX_NUM_ENV][PS_MAX_NR_IIDICC];
    int8_t ipd_par[PS_MAX_NUM_ENV][PS_MAX_NR_IIDICC];
    int8_t opd_par[PS_MAX_NUM_ENV][PS_MAX_NR_IIDICC];
    int    is34bands;
    int    is34bands_old;
} PSCommonContext;

static const int8_t num_env_tab_internal[2][4] = {
    { 0, 1, 2, 4, },
    { 1, 2, 3, 4, },
};
static const int8_t nr_iidicc_par_tab[] = { 10, 20, 34, 10, 20, 34 };
static const int8_t nr_ipdopd_par_tab[] = { 5, 11, 17, 5, 11, 17 };

#define READ_PAR_DATA(PAR, MASK, ERR_CONDITION, NB_BITS, MAX_DEPTH, TABIDX) \
static int read_ ## PAR ## _data(GetBitContext *gb, PSCommonContext *ps, \
                        int8_t (*PAR)[PS_MAX_NR_IIDICC], int table_idx, int e, int dt) \
{ \
    int b, num = ps->nr_ ## PAR ## _par; \
    (void)table_idx; \
    if (dt) { \
        int e_prev = e ? e - 1 : ps->num_env_old - 1; \
        e_prev = FFMAX(e_prev, 0); \
        for (b = 0; b < num; b++) { \
            int val = PAR[e_prev][b] + my_get_vlc2(gb, TABIDX); \
            if (MASK) val &= MASK; \
            PAR[e][b] = val; \
            if (trace_on) fprintf(stderr, "CVAL dt b=%d prev=%d val=%d\n", b, PAR[e_prev][b], val);\
            if (ERR_CONDITION) \
                goto err; \
        } \
    } else { \
        int val = 0; \
        for (b = 0; b < num; b++) { \
            val += my_get_vlc2(gb, TABIDX); \
            if (MASK) val &= MASK; \
            PAR[e][b] = val; \
            if (trace_on) fprintf(stderr, "CVAL df b=%d val=%d\n", b, val);\
            if (ERR_CONDITION) \
                goto err; \
        } \
    } \
    return 0; \
err: \
    return -1; \
}

READ_PAR_DATA(iid, 0, FFABS(ps->iid_par[e][b]) > 7 + 8 * ps->iid_quant, 9, 3, 2 * dt + ps->iid_quant)
READ_PAR_DATA(icc, 0, ps->icc_par[e][b] > 7U, 9, 2, 4 + dt)
READ_PAR_DATA(ipdopd, 0x07, 0, 5, 1, 6 + dt)

static int ps_read_extension_data(GetBitContext *gb, PSCommonContext *ps,
                                  int ps_extension_id)
{
    int e;
    int count = get_bits_count(gb);

    if (ps_extension_id)
        return 0;

    ps->enable_ipdopd = get_bits1(gb);
    if (ps->enable_ipdopd) {
        for (e = 0; e < ps->num_env; e++) {
            int dt = get_bits1(gb);
            read_ipdopd_data(gb, ps, ps->ipd_par, 0, e, dt);
            dt = get_bits1(gb);
            read_ipdopd_data(gb, ps, ps->opd_par, 0, e, dt);
        }
    }
    skip_bits1(gb);      /* reserved_ps */
    return get_bits_count(gb) - count;
}

int ff_ps_read_data_oracle(GetBitContext *gb_host,
                           PSCommonContext *ps, int bits_left)
{
    int e;
    int bit_count_start = get_bits_count(gb_host);
    int header;
    int bits_consumed;
    GetBitContext gbc = *gb_host, *gb = &gbc;

    header = get_bits1(gb);
    if (header) {
        ps->enable_iid = get_bits1(gb);
        if (ps->enable_iid) {
            int iid_mode = get_bits(gb, 3);
            if (iid_mode > 5) {
                goto err;
            }
            ps->nr_iid_par    = nr_iidicc_par_tab[iid_mode];
            ps->iid_quant     = iid_mode > 2;
            ps->nr_ipdopd_par = nr_ipdopd_par_tab[iid_mode];
        }
        ps->enable_icc = get_bits1(gb);
        if (ps->enable_icc) {
            int icc_mode = get_bits(gb, 3);
            if (icc_mode > 5) {
                goto err;
            }
            ps->nr_icc_par = nr_iidicc_par_tab[icc_mode];
        }
        ps->enable_ext = get_bits1(gb);
    }

    ps->frame_class = get_bits1(gb);
    ps->num_env_old = ps->num_env;
    ps->num_env     = num_env_tab_internal[ps->frame_class][get_bits(gb, 2)];

    ps->border_position[0] = -1;
    if (ps->frame_class) {
        for (e = 1; e <= ps->num_env; e++) {
            ps->border_position[e] = get_bits(gb, 5);
            if (ps->border_position[e] < ps->border_position[e - 1]) {
                goto err;
            }
        }
    } else
        for (e = 1; e <= ps->num_env; e++) {
            int lg2 = 0; int t = ps->num_env;
            while ((1 << (lg2 + 1)) <= t) lg2++;
            ps->border_position[e] = (e * numQMFSlots >> (t ? lg2 : 0)) - 1;
        }

    if (ps->enable_iid) {
        for (e = 0; e < ps->num_env; e++) {
            int dt = get_bits1(gb);
            if (read_iid_data(gb, ps, ps->iid_par, 2 * dt + ps->iid_quant, e, dt))
                goto err;
        }
    } else
        memset(ps->iid_par, 0, sizeof(ps->iid_par));

    if (ps->enable_icc)
        for (e = 0; e < ps->num_env; e++) {
            int dt = get_bits1(gb);
            if (read_icc_data(gb, ps, ps->icc_par, 4 + dt, e, dt))
                goto err;
        }
    else
        memset(ps->icc_par, 0, sizeof(ps->icc_par));

    if (ps->enable_ext) {
        int cnt = get_bits(gb, 4);
        if (cnt == 15) {
            cnt += get_bits(gb, 8);
        }
        cnt *= 8;
        while (cnt > 7) {
            int ps_extension_id = get_bits(gb, 2);
            cnt -= 2 + ps_read_extension_data(gb, ps, ps_extension_id);
        }
        if (cnt < 0) {
            goto err;
        }
        skip_bits(gb, cnt);
    }

    ps->enable_ipdopd &= !PS_BASELINE;

    if (!ps->num_env || ps->border_position[ps->num_env] < numQMFSlots - 1) {
        int source = ps->num_env ? ps->num_env - 1 : ps->num_env_old - 1;
        int b;
        if (source >= 0 && source != ps->num_env) {
            if (ps->enable_iid) {
                memcpy(ps->iid_par + ps->num_env, ps->iid_par + source, sizeof(ps->iid_par[0]));
            }
            if (ps->enable_icc) {
                memcpy(ps->icc_par + ps->num_env, ps->icc_par + source, sizeof(ps->icc_par[0]));
            }
            if (ps->enable_ipdopd) {
                memcpy(ps->ipd_par + ps->num_env, ps->ipd_par + source, sizeof(ps->ipd_par[0]));
                memcpy(ps->opd_par + ps->num_env, ps->opd_par + source, sizeof(ps->opd_par[0]));
            }
        }
        if (ps->enable_iid) {
            for (b = 0; b < ps->nr_iid_par; b++) {
                if (FFABS(ps->iid_par[ps->num_env][b]) > 7 + 8 * ps->iid_quant) {
                    goto err;
                }
            }
        }
        if (ps->enable_icc) {
            for (b = 0; b < ps->nr_iid_par; b++) {
                if (ps->icc_par[ps->num_env][b] > 7U) {
                    goto err;
                }
            }
        }
        ps->num_env++;
        ps->border_position[ps->num_env] = numQMFSlots - 1;
    }

    ps->is34bands_old = ps->is34bands;
    if (!PS_BASELINE && (ps->enable_iid || ps->enable_icc))
        ps->is34bands = (ps->enable_iid && ps->nr_iid_par == 34) ||
                        (ps->enable_icc && ps->nr_icc_par == 34);

    if (!ps->enable_ipdopd) {
        memset(ps->ipd_par, 0, sizeof(ps->ipd_par));
        memset(ps->opd_par, 0, sizeof(ps->opd_par));
    }

    if (header)
        ps->start = 1;

    bits_consumed = get_bits_count(gb) - bit_count_start;
    if (bits_consumed <= bits_left) {
        skip_bits_long(gb_host, bits_consumed);
        return bits_consumed;
    }
err:
    ps->start = 0;
    skip_bits_long(gb_host, bits_left);
    memset(ps->iid_par, 0, sizeof(ps->iid_par));
    memset(ps->icc_par, 0, sizeof(ps->icc_par));
    memset(ps->ipd_par, 0, sizeof(ps->ipd_par));
    memset(ps->opd_par, 0, sizeof(ps->opd_par));
    return bits_left;
}

int main(int argc, char **argv)
{
    char line[4096];
    for (int i = 0; i < 10; i++) build_canonical(&vlc_ps_internal[i], i);

    FILE *f = argc > 1 ? fopen(argv[1], "r") : stdin;
    if (!f) { perror("open"); return 1; }
    PSCommonContext ps;
    memset(&ps, 0, sizeof(ps));
    int block_idx = 0;
    while (fgets(line, sizeof(line), f)) {
        int n = strlen(line);
        while (n && (line[n - 1] == '\n' || line[n - 1] == '\r')) line[--n] = 0;
        if (!n) continue;
        int nbytes = (n + 7) / 8;
        uint8_t *buf = calloc(nbytes + 8, 1);
        for (int i = 0; i < n; i++)
            if (line[i] == '1') buf[i >> 3] |= 0x80 >> (i & 7);
        GetBitContext gb;
        init_get_bits8(&gb, buf, nbytes + 8);
        trace_on = (block_idx == 3);
        trace_pos_base = 0;
        int consumed = ff_ps_read_data_oracle(&gb, &ps, n);
        block_idx++;
        printf("PSO consumed=%d env=%d start=%d borders=[", consumed, ps.num_env, ps.start);
        for (int e = 0; e <= ps.num_env; e++)
            printf("%d%s", ps.border_position[e], e < ps.num_env ? "," : "");
        printf("] iid_q=%d nr_iid=%d nr_icc=%d nr_ipdopd=%d ipdopd=%d is34=%d old=%d\n",
               ps.iid_quant, ps.nr_iid_par, ps.nr_icc_par, ps.nr_ipdopd_par,
               ps.enable_ipdopd, ps.is34bands, ps.is34bands_old);
        for (int e = 0; e < ps.num_env; e++) {
            printf("OIID e=%d:", e);
            for (int b = 0; b < 34; b++) printf(" %d", ps.iid_par[e][b]);
            printf("\n");
            printf("OICC e=%d:", e);
            for (int b = 0; b < 34; b++) printf(" %d", ps.icc_par[e][b]);
            printf("\n");
            printf("OIPD e=%d:", e);
            for (int b = 0; b < 34; b++) printf(" %d", ps.ipd_par[e][b]);
            printf("\n");
            printf("OOPD e=%d:", e);
            for (int b = 0; b < 34; b++) printf(" %d", ps.opd_par[e][b]);
            printf("\n");
        }
        free(buf);
    }
    return 0;
}
