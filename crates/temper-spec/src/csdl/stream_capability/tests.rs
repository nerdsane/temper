use super::*;
use crate::csdl::parse_csdl;

const VALID: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.FS" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="File" HasStream="true">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
        <NavigationProperty Name="Versions" Type="Collection(Temper.FS.FileVersion)"/>
        <Annotation Term="Temper.Vocab.Stream.Mutability" String="Mutable"/>
        <Annotation Term="Temper.Vocab.Stream.VersionEntityType" String="Temper.FS.FileVersion"/>
        <Annotation Term="Temper.Vocab.Stream.VersionCollection" NavigationPropertyPath="Versions"/>
        <Annotation Term="Temper.Vocab.Stream.MigrationPublicationAction" String="StreamUpdated"/>
        <Annotation Term="Temper.Vocab.Stream.MigrationContentHashParameter" String="content_hash"/>
        <Annotation Term="Temper.Vocab.Stream.MigrationByteLengthParameter" String="size_bytes"/>
        <Annotation Term="Temper.Vocab.Stream.MigrationContentTypeParameter" String="mime_type"/>
        <Annotation Term="Temper.Vocab.Stream.MigrationStorageContractVersion" Int="1"/>
        <Annotation Term="Temper.Vocab.Stream.MigrationStorageKeyPrefix" String="temper-fs/"/>
      </EntityType>
      <EntityType Name="FileVersion">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
        <Property Name="FileId" Type="Edm.Guid" Nullable="false"/>
        <NavigationProperty Name="File" Type="Temper.FS.File">
          <ReferentialConstraint Property="FileId" ReferencedProperty="Id"/>
        </NavigationProperty>
        <Annotation Term="Temper.Vocab.Stream.Mutability" String="Immutable"/>
        <Annotation Term="Temper.Vocab.Stream.AuthorizationParent" NavigationPropertyPath="File"/>
        <Annotation Term="Temper.Vocab.Stream.MigrationPublicationAction" String="Create"/>
        <Annotation Term="Temper.Vocab.Stream.MigrationContentHashParameter" String="content_hash"/>
        <Annotation Term="Temper.Vocab.Stream.MigrationByteLengthParameter" String="size_bytes"/>
        <Annotation Term="Temper.Vocab.Stream.MigrationContentTypeParameter" String="mime_type"/>
        <Annotation Term="Temper.Vocab.Stream.MigrationAuthorizationParentParameter" String="file_id"/>
        <Annotation Term="Temper.Vocab.Stream.MigrationStorageContractVersion" Int="1"/>
        <Annotation Term="Temper.Vocab.Stream.MigrationStorageKeyPrefix" String="temper-fs/"/>
      </EntityType>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

#[test]
fn verifies_mutual_current_and_version_capabilities() {
    let capabilities = verify_stream_capabilities_v1(&parse_csdl(VALID).unwrap()).unwrap();
    assert_eq!(capabilities.len(), 2);
    assert_eq!(capabilities[0].subject_type, "Temper.FS.File");
    assert_eq!(
        capabilities[0].version_entity_type.as_deref(),
        Some("Temper.FS.FileVersion")
    );
    assert_eq!(
        capabilities[1].authorization_parent_type.as_deref(),
        Some("Temper.FS.File")
    );
    assert_eq!(
        capabilities[1]
            .migration_provenance
            .as_ref()
            .and_then(|provenance| provenance.authorization_parent_parameter.as_deref()),
        Some("file_id")
    );
    assert_eq!(
        stream_capability_set_digest_v1(&capabilities).unwrap(),
        stream_capability_set_digest_v1(&capabilities.iter().rev().cloned().collect::<Vec<_>>())
            .unwrap()
    );
}

#[test]
fn rejects_string_in_place_of_navigation_property_path() {
    let invalid = VALID.replace("NavigationPropertyPath=\"Versions\"", "String=\"Versions\"");
    assert!(matches!(
        verify_stream_capabilities_v1(&parse_csdl(&invalid).unwrap()),
        Err(StreamCapabilityError::InvalidAnnotationValue { .. })
    ));
}

#[test]
fn rejects_non_mutual_and_unconstrained_parent_contracts() {
    let non_mutual = VALID.replace("Type=\"Temper.FS.File\"", "Type=\"Temper.FS.Other\"");
    assert!(verify_stream_capabilities_v1(&parse_csdl(&non_mutual).unwrap()).is_err());

    let unconstrained = VALID.replace(
        "<ReferentialConstraint Property=\"FileId\" ReferencedProperty=\"Id\"/>",
        "",
    );
    assert!(matches!(
        verify_stream_capabilities_v1(&parse_csdl(&unconstrained).unwrap()),
        Err(StreamCapabilityError::InvalidReferentialConstraint { .. })
    ));
}

#[test]
fn descriptor_contract_activation_is_distinct_and_closed() {
    let inactive = verify_stream_capabilities_v1(&parse_csdl(VALID).unwrap()).unwrap();
    assert!(
        inactive
            .iter()
            .all(|capability| !capability.descriptor_contract_v1_active)
    );
    let active_xml = VALID.replace(
        "<Annotation Term=\"Temper.Vocab.Stream.Mutability\" String=\"Mutable\"/>",
        "<Annotation Term=\"Temper.Vocab.Stream.Mutability\" String=\"Mutable\"/>\n        <Annotation Term=\"Temper.Vocab.Stream.DescriptorContractVersion\" Int=\"1\"/>",
    );
    let active = verify_stream_capabilities_v1(&parse_csdl(&active_xml).unwrap()).unwrap();
    assert!(active[0].descriptor_contract_v1_active);
    let unsupported = active_xml.replace(
        "DescriptorContractVersion\" Int=\"1\"",
        "DescriptorContractVersion\" Int=\"2\"",
    );
    assert!(matches!(
        verify_stream_capabilities_v1(&parse_csdl(&unsupported).unwrap()),
        Err(StreamCapabilityError::UnsupportedDescriptorContract { .. })
    ));
}

#[test]
fn activated_contract_requires_complete_migration_provenance() {
    let without_provenance = VALID
        .lines()
        .filter(|line| !line.contains("Stream.Migration"))
        .collect::<Vec<_>>()
        .join("\n")
        .replace(
            "<Annotation Term=\"Temper.Vocab.Stream.Mutability\" String=\"Mutable\"/>",
            "<Annotation Term=\"Temper.Vocab.Stream.Mutability\" String=\"Mutable\"/>\n        <Annotation Term=\"Temper.Vocab.Stream.DescriptorContractVersion\" Int=\"1\"/>",
        );
    assert!(matches!(
        verify_stream_capabilities_v1(&parse_csdl(&without_provenance).unwrap()),
        Err(StreamCapabilityError::MissingMigrationProvenance(_))
    ));

    let missing_parent = VALID.replace(
        "        <Annotation Term=\"Temper.Vocab.Stream.MigrationAuthorizationParentParameter\" String=\"file_id\"/>\n",
        "",
    );
    assert!(matches!(
        verify_stream_capabilities_v1(&parse_csdl(&missing_parent).unwrap()),
        Err(StreamCapabilityError::MigrationParentBinding { .. })
    ));
}
