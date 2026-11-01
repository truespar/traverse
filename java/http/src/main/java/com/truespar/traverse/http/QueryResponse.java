package com.truespar.traverse.http;

import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
import com.fasterxml.jackson.annotation.JsonProperty;
import tools.jackson.databind.JsonNode;
import java.util.List;

/**
 * Query result from {@code POST /api/query}.
 * Row values are {@link JsonNode} - use {@code .asInt()}, {@code .asText()}, etc. to extract.
 */
@JsonIgnoreProperties(ignoreUnknown = true)
public record QueryResponse(
        List<String> columns,
        List<List<JsonNode>> rows,
        @JsonProperty("total_rows") long totalRows,
        boolean truncated,
        @JsonProperty("query_type") String queryType,
        Stats stats,
        @JsonProperty("time_ms") double timeMs,
        @JsonProperty("parsing_time_ms") double parsingTimeMs,
        @JsonProperty("planning_time_ms") double planningTimeMs,
        @JsonProperty("execution_time_ms") double executionTimeMs) {

    @JsonIgnoreProperties(ignoreUnknown = true)
    public record Stats(
            @JsonProperty("nodes_created") long nodesCreated,
            @JsonProperty("nodes_deleted") long nodesDeleted,
            @JsonProperty("relationships_created") long relationshipsCreated,
            @JsonProperty("relationships_deleted") long relationshipsDeleted,
            @JsonProperty("properties_set") long propertiesSet,
            @JsonProperty("labels_added") long labelsAdded,
            @JsonProperty("labels_removed") long labelsRemoved) {}
}
