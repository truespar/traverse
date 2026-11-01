package com.truespar.traverse.http;

import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
import com.fasterxml.jackson.annotation.JsonProperty;
import java.util.List;

@JsonIgnoreProperties(ignoreUnknown = true)
public record SchemaResponse(
        List<String> labels,
        @JsonProperty("relationship_types") List<String> relationshipTypes,
        @JsonProperty("property_keys") List<Property> propertyKeys,
        List<Index> indexes,
        List<Constraint> constraints,
        @JsonProperty("labels_detail") List<LabelDetail> labelsDetail,
        @JsonProperty("relationship_types_detail") List<LabelDetail> relationshipTypesDetail) {

    @JsonIgnoreProperties(ignoreUnknown = true)
    public record Property(String name, String type) {}

    @JsonIgnoreProperties(ignoreUnknown = true)
    public record Index(String name, String label, String property, long entries) {}

    @JsonIgnoreProperties(ignoreUnknown = true)
    public record Constraint(String name, String type, String label, String property) {}

    @JsonIgnoreProperties(ignoreUnknown = true)
    public record LabelDetail(String name, List<Property> properties) {}
}
