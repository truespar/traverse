package com.truespar.traverse.http;

import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
import com.fasterxml.jackson.annotation.JsonProperty;
import java.util.List;
import java.util.Map;

/**
 * The server's health ({@code GET /api/health}).
 *
 * @param features compile-time server capabilities, e.g. {gql: true, ml: false}; check
 *     {@code features().get("gql")} before passing dialect "gql" to query()
 */
@JsonIgnoreProperties(ignoreUnknown = true)
public record HealthResponse(
        String status,
        String version,
        @JsonProperty("uptime_secs") long uptimeSecs,
        @JsonProperty("memory_bytes") long memoryBytes,
        List<Database> databases,
        @JsonProperty("allow_drop_database") boolean allowDropDatabase,
        @JsonProperty("auth_required") boolean authRequired,
        Map<String, Boolean> features) {

    @JsonIgnoreProperties(ignoreUnknown = true)
    public record Database(
            String name,
            @JsonProperty("default") boolean isDefault,
            long nodes,
            long edges,
            @JsonProperty("memory_bytes") long memoryBytes) {}
}
