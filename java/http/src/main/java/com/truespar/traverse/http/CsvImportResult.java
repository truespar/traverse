package com.truespar.traverse.http;

import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.annotation.JsonProperty;
import java.util.List;

@JsonIgnoreProperties(ignoreUnknown = true)
public record CsvImportResult(
        @JsonProperty("nodes_created") long nodesCreated,
        @JsonProperty("properties_set") long propertiesSet,
        @JsonProperty("labels_added") long labelsAdded,
        List<Error> errors,
        @JsonProperty("time_ms") double timeMs) {

    @JsonIgnoreProperties(ignoreUnknown = true)
    public record Error(long row, String error) {}

    @JsonIgnoreProperties(ignoreUnknown = true)
    public record EdgeResult(
            @JsonProperty("edges_created") long edgesCreated,
            @JsonProperty("properties_set") long propertiesSet,
            List<Error> errors,
            @JsonProperty("time_ms") double timeMs) {}

    @JsonInclude(JsonInclude.Include.NON_NULL)
    public record ColumnMapping(String name, String property, String type) {}

    public record EdgeEndpoint(String label, String column, String property) {}
}
