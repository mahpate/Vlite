#include "vlite.h"
#include <stdio.h>
#include <string.h>

int main(void) {
    const char *request = "{\"op\":\"info\"}";
    if (vlite_abi_version() != 1) return 1;
    /* Handle 0 is invalid for info: verify a structured ABI response. */
    char *response = vlite_execute(0, (const uint8_t *)request, strlen(request));
    if (response == NULL) return 2;
    int valid = strstr(response, "\"invalid_handle\"") != NULL;
    puts(response);
    vlite_response_free(response);
    return valid ? 0 : 3;
}
