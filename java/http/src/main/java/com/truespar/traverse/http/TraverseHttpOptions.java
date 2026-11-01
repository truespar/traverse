package com.truespar.traverse.http;

import java.time.Duration;

/**
 * Configuration options for {@link TraverseHttpClient}.
 *
 * @param apiKey              API key sent as {@code Authorization: Bearer {key}}. Null to skip auth.
 * @param timeout             HTTP timeout for API calls. Default: 100 seconds.
 * @param fileTransferTimeout Timeout for file upload/download. Default: 1 hour.
 * @param allowUntrustedCertificate Allow connections to servers with untrusted TLS certificates.
 */
public record TraverseHttpOptions(
        String apiKey,
        Duration timeout,
        Duration fileTransferTimeout,
        boolean allowUntrustedCertificate) {

    public TraverseHttpOptions() {
        this(null, null, null, false);
    }

    public TraverseHttpOptions(String apiKey) {
        this(apiKey, null, null, false);
    }
}
