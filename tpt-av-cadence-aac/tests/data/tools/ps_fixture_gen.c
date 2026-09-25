/* HE-AACv2 (AOT 29, Parametric Stereo) fixture generator.
 * Encodes one second of a deterministic mono two-tone (440 + 660 Hz at the
 * 24 kHz AAC core rate) to a 32 kbps HE-AACv2 ADTS stream with libfdk-aac,
 * the same toolchain/provenance convention as the crate's other SBR
 * fixtures. Build (from the fdk-aac build directory):
 *   gcc -O2 -I<fdk-src>/libAACenc -I<fdk-src>/libSYS -I<fdk-src>/libFDK \
 *       -I<fdk-src>/libMpegTPDec -I<fdk-src>/libMpegTPEnc -I<fdk-src>/libSBRenc \
 *       ps_fixture_gen.c build/libfdk-aac.a -o ps_fixture_gen
 */
#include <stdio.h>
#include <stdlib.h>
#include <math.h>
#include "aacenc_lib.h"

int main(int argc, char **argv)
{
    HANDLE_AACENCODER enc;
    AACENC_InfoStruct info = { 0 };
    const int core_rate = 24000;
    const int n_samples = core_rate; /* 1 second */
    static short pcm[24000 * 2];
    static unsigned char outbuf[2048];
    int i, err;

    if (argc < 2) { fprintf(stderr, "usage: %s <out.aac>\n", argv[0]); return 1; }

    for (i = 0; i < n_samples; i++) {
        double l = 0.5 * sin(2.0 * M_PI * 440.0 * i / core_rate)
                 + 0.3 * sin(2.0 * M_PI * 660.0 * i / core_rate);
        /* slight L/R level tilt so the PS encoder has real IID/ICC to
         * describe; deterministic, both channels derived from the same
         * oscillator phase */
        double r = 0.8 * l;
        double v = l * 32767.0;
        double w = r * 32767.0;
        pcm[2 * i] = (short)(v < -32768.0 ? -32768.0 : v > 32767.0 ? 32767.0 : v);
        pcm[2 * i + 1] = (short)(w < -32768.0 ? -32768.0 : w > 32767.0 ? 32767.0 : w);
    }

    if ((err = aacEncOpen(&enc, 0, 2)) != AACENC_OK) { fprintf(stderr, "aacEncOpen %d\n", err); return 1; }
    if (aacEncoder_SetParam(enc, AACENC_AOT, 29) != AACENC_OK) { fprintf(stderr, "AOT\n"); return 1; }
    if (aacEncoder_SetParam(enc, AACENC_SAMPLERATE, core_rate) != AACENC_OK) { fprintf(stderr, "SR\n"); return 1; }
    if (aacEncoder_SetParam(enc, AACENC_CHANNELMODE, 2) != AACENC_OK) { fprintf(stderr, "CH\n"); return 1; }
    if (aacEncoder_SetParam(enc, AACENC_BITRATE, 32000) != AACENC_OK) { fprintf(stderr, "BR\n"); return 1; }
    if (aacEncoder_SetParam(enc, AACENC_TRANSMUX, TT_MP4_ADTS) != AACENC_OK) { fprintf(stderr, "TT\n"); return 1; }
    if (aacEncoder_SetParam(enc, AACENC_AFTERBURNER, 1) != AACENC_OK) { fprintf(stderr, "AB\n"); return 1; }
    if ((err = aacEncEncode(enc, NULL, NULL, NULL, NULL)) != AACENC_OK) { fprintf(stderr, "init %d\n", err); return 1; }
    if (aacEncInfo(enc, &info) != AACENC_OK) { fprintf(stderr, "info\n"); return 1; }

    FILE *f = fopen(argv[1], "wb");
    if (!f) { perror(argv[1]); return 1; }

    AACENC_BufDesc in_desc = { 0 }, out_desc = { 0 };
    AACENC_InArgs in_args = { 0 };
    int in_ident = IN_AUDIO_DATA, out_ident = OUT_BITSTREAM_DATA;
    int in_size = 0, in_elem = 2 /* sizeof(short) per sample */, out_size = sizeof(outbuf), out_elem = 1;
    AACENC_OutArgs out_args = { 0 };

    i = 0;
    while (i < n_samples) {
        int frame = info.frameLength; /* 1024 core samples */
        if (i + frame > n_samples) frame = n_samples - i;
        in_args.numInSamples = frame * 2;
        in_size = frame * 2 * (int)sizeof(short);
        in_desc.numBufs = 1;
        in_desc.bufs = (void **)&pcm;
        in_desc.bufferIdentifiers = &in_ident;
        in_desc.bufSizes = &in_size;
        in_desc.bufElSizes = &in_elem;
        /* the encoder consumes from the buffer start each call; feed a
         * moving pointer */
        short *cur = pcm + i;
        in_desc.bufs = (void **)&cur;
        out_desc.numBufs = 1;
        out_desc.bufs = (void **)&outbuf;
        out_desc.bufferIdentifiers = &out_ident;
        out_desc.bufSizes = &out_size;
        out_desc.bufElSizes = &out_elem;
        err = aacEncEncode(enc, &in_desc, &out_desc, &in_args, &out_args);
        if (err != AACENC_OK) { fprintf(stderr, "encode %d at %d\n", err, i); return 1; }
        if (out_args.numOutBytes > 0) fwrite(outbuf, 1, out_args.numOutBytes, f);
        i += frame;
        in_args.numInSamples = 0; /* flush tail below */
    }
    /* flush */
    in_args.numInSamples = -1;
    out_desc.bufs = (void **)&outbuf;
    err = aacEncEncode(enc, &in_desc, &out_desc, &in_args, &out_args);
    if (err == AACENC_OK && out_args.numOutBytes > 0) fwrite(outbuf, 1, out_args.numOutBytes, f);
    fclose(f);
    aacEncClose(&enc);
    return 0;
}
