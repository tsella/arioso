/*
 * The C ABI, called from C.
 *
 * This is the part the Rust tests cannot prove: that a C compiler can read include/arioso.h,
 * that the symbols exist in libarioso.a with the signatures the header declares, and that a
 * caller who has never seen the Rust code can drive it and get sentences back.
 *
 * It builds a models directory out of nothing — a manifest with no models in it and an empty
 * dictionary — so engine creation is real, not a stub. Synthesis then fails, and that is the
 * assertion: with no voice installed the call must refuse cleanly, with a code, a sentence,
 * and a null buffer, rather than pretend to speak. A synthesis test with real audio needs a
 * downloaded voice, which is the Rust suite's job (`ARIOSO_TEST_MODELS_DIR`), not this one's.
 *
 * POSIX only: it uses mkdtemp, so CI compiles and runs it on Linux and macOS and skips it on
 * Windows, where the C runtime has no such function and `cc` is not guaranteed to exist.
 */

#define _POSIX_C_SOURCE 200809L

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "arioso.h"

static int failures;

/* Report one check, with the sentence the library left, when there is one. */
static void check(int ok, const char *what, const char *detail) {
    if (ok) {
        printf("ok    %s\n", what);
    } else {
        printf("FAIL  %s%s%s\n", what, detail && *detail ? ": " : "", detail ? detail : "");
        failures++;
    }
}

static int write_file(const char *path, const char *text) {
    FILE *file = fopen(path, "w");
    if (file == NULL) {
        return 0;
    }
    fputs(text, file);
    fclose(file);
    return 1;
}

int main(void) {
    printf("arioso C ABI smoke test\n");

    /* The version is the first thing a caller can check, and the only thing that cannot
     * fail: if this links, the header and the library are the same surface. */
    {
        char detail[64];
        uint32_t version = arioso_abi_version();
        snprintf(detail, sizeof detail, "got %u", version);
        check(version == ARIOSO_ABI_VERSION, "arioso_abi_version() is the header's version",
              detail);
        printf("      arioso_abi_version() = %u\n", version);
    }

    /* A models directory an engine can load: a catalog with no models, and a dictionary with
     * no entries. Nothing here is a fake engine; it is an empty install. */
    const char *tmp = getenv("TMPDIR");
    char dir[512];
    snprintf(dir, sizeof dir, "%s/arioso-smoke-XXXXXX", tmp != NULL && *tmp ? tmp : "/tmp");
    if (mkdtemp(dir) == NULL) {
        perror("mkdtemp");
        return 1;
    }

    char manifest[640];
    char dictionary[640];
    snprintf(manifest, sizeof manifest, "%s/manifest.json", dir);
    snprintf(dictionary, sizeof dictionary, "%s/cmudict.dict", dir);
    if (!write_file(manifest, "{\"schema_version\": 1, \"models\": []}\n") ||
        !write_file(dictionary, "")) {
        perror("writing the models directory");
        return 1;
    }

    char config[768];
    snprintf(config, sizeof config, "{\"models\": {\"dir\": \"%s\"}}", dir);

    AriosoEngine *engine = NULL;
    int32_t code = arioso_engine_new(config, &engine);
    check(code == ARIOSO_OK && engine != NULL, "an engine is created from a config",
          arioso_last_error());
    if (code != ARIOSO_OK) {
        printf("      %s\n", arioso_last_error());
        return 1;
    }

    /* A config the rules refuse. The code is for the program; the sentence is for the person
     * reading the log, and it names the field. */
    {
        AriosoEngine *refused = NULL;
        code = arioso_engine_new("{\"server\": {\"bind\": \"not an address\"}}", &refused);
        printf("      %d %s\n", code, arioso_last_error());
        check(code == ARIOSO_ERR_CONFIG && refused == NULL,
              "a bad config is refused with ARIOSO_ERR_CONFIG", NULL);
        check(strstr(arioso_last_error(), "server.bind") != NULL,
              "the sentence names the field that is wrong", arioso_last_error());
    }

    /* Null arguments are codes, not crashes: a C caller's cleanup path is allowed to be
     * careless, and a crash there is the worst possible failure. */
    {
        AriosoEngine *never = NULL;
        code = arioso_engine_new(NULL, &never);
        check(code == ARIOSO_ERR_NULL_ARGUMENT, "a null config is ARIOSO_ERR_NULL_ARGUMENT",
              arioso_last_error());
        code = arioso_engine_new(config, NULL);
        check(code == ARIOSO_ERR_NULL_ARGUMENT, "a null out pointer is a code",
              arioso_last_error());
        code = arioso_tts_synthesize(NULL, "hello", NULL, NULL);
        check(code == ARIOSO_ERR_NULL_ARGUMENT, "a null engine is a code", arioso_last_error());
    }

    /* No voice is installed, so synthesis must refuse. The point is the shape of the refusal:
     * a code, a sentence, and a buffer with nothing in it for the caller to free. */
    {
        AriosoBuffer wav = {0};
        code = arioso_tts_synthesize(engine, "hello from C", NULL, &wav);
        printf("      %d %s\n", code, arioso_last_error());
        check(code < 0, "synthesis without a voice is an error code", NULL);
        check(wav.data == NULL && wav.len == 0, "a refused synthesis hands back no buffer",
              NULL);
        check(strlen(arioso_last_error()) > 0, "the refusal has a sentence", NULL);
    }

    /* An options struct from a newer header than the library: refused by size, never read
     * past its end. */
    {
        AriosoBuffer wav = {0};
        AriosoTtsOptions options;
        memset(&options, 0, sizeof options);
        options.struct_size = sizeof(AriosoTtsOptions) + 8;
        options.speed = 1.0f;
        code = arioso_tts_synthesize(engine, "hello from C", &options, &wav);
        printf("      %d %s\n", code, arioso_last_error());
        check(code == ARIOSO_ERR_STRUCT_SIZE, "a struct_size the library does not know is refused",
              NULL);
    }

    /* The releases. Both take null as a no-op, so a caller that never got a buffer can still
     * run the same cleanup. */
    arioso_free(NULL);
    arioso_engine_free(engine);
    arioso_engine_free(NULL);
    check(arioso_last_error()[0] == '\0', "the error string is empty after a clean call", NULL);

    remove(dictionary);
    remove(manifest);
    rmdir(dir);

    if (failures != 0) {
        printf("%d check(s) failed\n", failures);
        return 1;
    }
    printf("all checks passed\n");
    return 0;
}
