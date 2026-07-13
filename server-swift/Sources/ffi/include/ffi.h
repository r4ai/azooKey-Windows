#ifndef AZOOKEY_SERVER_FFI_H
#define AZOOKEY_SERVER_FFI_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

enum AzookeyFFIStatus {
    AZOOKEY_FFI_SUCCESS = 0,
    AZOOKEY_FFI_INVALID_ARGUMENT = 1,
    AZOOKEY_FFI_NOT_INITIALIZED = 2,
    AZOOKEY_FFI_ALLOCATION_FAILED = 3,
};

struct FFICandidate {
    char *text;
    char *subtext;
    int32_t correspondingCount;
};

/*
 * All returned strings and candidate lists are owned by this library.
 * The caller must release them with FreeCString/FreeCandidateList.
 * A NULL candidate list with length 0 is a valid empty result.
 */
int32_t Initialize(const char *path, int32_t useZenzai);
int32_t LoadConfig(void);
int32_t SetContext(const char *context);

char *AppendText(const char *input, int32_t *cursor);
char *RemoveText(int32_t *cursor);
char *MoveCursor(int32_t offset, int32_t *cursor);
char *ShrinkText(int32_t offset);
char *CommitPrefixAndAppend(int32_t offset, const char *input, int32_t *cursor);
void ClearText(void);

struct FFICandidate **GetComposedText(int32_t *length);
char *GetRawInput(void);
void FreeCString(char *string);
void FreeCandidateList(struct FFICandidate **candidates, int32_t length);

#ifdef __cplusplus
}
#endif

#endif /* AZOOKEY_SERVER_FFI_H */
