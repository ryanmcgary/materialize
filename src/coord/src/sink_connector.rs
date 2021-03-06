// Copyright Materialize, Inc. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::fs::OpenOptions;
use std::time::Duration;

use anyhow::{anyhow, Context};
use rdkafka::admin::{AdminClient, AdminOptions, NewTopic, ResourceSpecifier, TopicReplication};
use rdkafka::client::DefaultClientContext;
use rdkafka::config::ClientConfig;

use dataflow_types::{
    AvroOcfSinkConnector, AvroOcfSinkConnectorBuilder, KafkaSinkConnector,
    KafkaSinkConnectorBuilder, KafkaSinkConsistencyConnector, SinkConnector, SinkConnectorBuilder,
};
use expr::GlobalId;
use ore::collections::CollectionExt;

use crate::error::CoordError;

pub async fn build(
    builder: SinkConnectorBuilder,
    id: GlobalId,
) -> Result<SinkConnector, CoordError> {
    match builder {
        SinkConnectorBuilder::Kafka(k) => build_kafka(k, id).await,
        SinkConnectorBuilder::AvroOcf(a) => build_avro_ocf(a, id),
    }
}

async fn register_kafka_topic(
    client: &AdminClient<DefaultClientContext>,
    topic: &str,
    mut partition_count: i32,
    mut replication_factor: i32,
    ccsr: &ccsr::Client,
    value_schema: &str,
    key_schema: Option<&str>,
) -> Result<(Option<i32>, i32), CoordError> {
    // if either partition count or replication factor should be defaulted to the broker's config
    // (signaled by a value of -1), explicitly poll the broker to discover the defaults.
    // Newer versions of Kafka can instead send create topic requests with -1 and have this happen
    // behind the scenes, but this is unsupported and will result in errors on pre-2.4 Kafka
    if partition_count == -1 || replication_factor == -1 {
        let metadata = client
            .inner()
            .fetch_metadata(None, Duration::from_secs(5))
            .with_context(|| {
                format!(
                    "error fetching metadata when creating new topic {} for sink",
                    topic
                )
            })?;

        if metadata.brokers().len() == 0 {
            coord_bail!("zero brokers discovered in metadata request");
        }

        let broker = metadata.brokers()[0].id();
        let configs = client
            .describe_configs(
                &[ResourceSpecifier::Broker(broker)],
                &AdminOptions::new().request_timeout(Some(Duration::from_secs(5))),
            )
            .await
            .with_context(|| {
                format!(
                    "error fetching configuration from broker {} when creating new topic {} for sink",
                    broker,
                    topic
                )
        })?;

        if configs.len() != 1 {
            coord_bail!(
                "error creating topic {} for sink: broker {} returned {} config results, but one was expected",
                topic,
                broker,
                configs.len()
            );
        }

        let config = configs.into_element().map_err(|e| {
            anyhow!(
                "error reading broker configuration when creating topic {} for sink: {}",
                topic,
                e
            )
        })?;

        for entry in config.entries {
            if entry.name == "num.partitions" && partition_count == -1 {
                if let Some(s) = entry.value {
                    partition_count = s.parse::<i32>().with_context(|| {
                        format!(
                            "default partition count {} cannot be parsed into an integer",
                            s
                        )
                    })?;
                }
            } else if entry.name == "default.replication.factor" && replication_factor == -1 {
                if let Some(s) = entry.value {
                    replication_factor = s.parse::<i32>().with_context(|| {
                        format!(
                            "default replication factor {} cannot be parsed into an integer",
                            s
                        )
                    })?;
                }
            }
        }

        if partition_count == -1 {
            coord_bail!("default was requested for partition_count, but num.partitions was not found in broker config");
        }

        if replication_factor == -1 {
            coord_bail!("default was requested for replication_factor, but default.replication.factor was not found in broker config");
        }
    }

    let res = client
        .create_topics(
            &[NewTopic::new(
                &topic,
                partition_count,
                TopicReplication::Fixed(replication_factor),
            )],
            &AdminOptions::new().request_timeout(Some(Duration::from_secs(5))),
        )
        .await
        .with_context(|| format!("error creating new topic {} for sink", topic))?;
    if res.len() != 1 {
        coord_bail!(
            "error creating topic {} for sink: \
             kafka topic creation returned {} results, but exactly one result was expected",
            topic,
            res.len()
        );
    }
    res.into_element()
        .map_err(|(_, e)| anyhow!("error creating topic {} for sink: {}", topic, e))?;

    // Publish value schema for the topic.
    //
    // TODO(benesch): do we need to delete the Kafka topic if publishing the
    // schema fails?
    let value_schema_id = ccsr
        .publish_schema(&format!("{}-value", topic), value_schema)
        .await
        .context("unable to publish value schema to registry in kafka sink")?;

    let key_schema_id = if let Some(key_schema) = key_schema {
        Some(
            ccsr.publish_schema(&format!("{}-key", topic), key_schema)
                .await
                .context("unable to publish key schema to registry in kafka sink")?,
        )
    } else {
        None
    };

    Ok((key_schema_id, value_schema_id))
}

async fn build_kafka(
    builder: KafkaSinkConnectorBuilder,
    id: GlobalId,
) -> Result<SinkConnector, CoordError> {
    let topic = format!("{}-{}-{}", builder.topic_prefix, id, builder.topic_suffix);

    // Create Kafka topic with single partition.
    let mut config = ClientConfig::new();
    config.set("bootstrap.servers", &builder.broker_addrs.to_string());
    for (k, v) in builder.config_options.iter() {
        config.set(k, v);
    }
    let client = config
        .create::<AdminClient<_>>()
        .expect("creating admin client failed");
    let ccsr = builder.ccsr_config.build();

    let (key_schema_id, value_schema_id) = register_kafka_topic(
        &client,
        &topic,
        builder.partition_count,
        builder.replication_factor,
        &ccsr,
        &builder.value_schema,
        builder.key_schema.as_deref(),
    )
    .await
    .context("error registering kafka topic for sink")?;

    let consistency = if let Some(consistency_value_schema) = builder.consistency_value_schema {
        let consistency_topic = format!("{}-consistency", topic);
        let (_, consistency_schema_id) = register_kafka_topic(
            &client,
            &consistency_topic,
            1,
            builder.replication_factor,
            &ccsr,
            &consistency_value_schema,
            None,
        )
        .await
        .context("error registering kafka consistency topic for sink")?;

        Some(KafkaSinkConsistencyConnector {
            topic: consistency_topic,
            schema_id: consistency_schema_id,
        })
    } else {
        None
    };

    Ok(SinkConnector::Kafka(KafkaSinkConnector {
        key_schema_id,
        value_schema_id,
        topic,
        addrs: builder.broker_addrs,
        key_desc_and_indices: builder.key_desc_and_indices,
        value_desc: builder.value_desc,
        consistency,
        fuel: builder.fuel,
        config_options: builder.config_options,
    }))
}

fn build_avro_ocf(
    builder: AvroOcfSinkConnectorBuilder,
    id: GlobalId,
) -> Result<SinkConnector, CoordError> {
    let mut name = match builder.path.file_stem() {
        None => coord_bail!(
            "unable to read file name from path {}",
            builder.path.display()
        ),
        Some(stem) => stem.to_owned(),
    };
    name.push("-");
    name.push(id.to_string());
    name.push("-");
    name.push(builder.file_name_suffix);
    if let Some(extension) = builder.path.extension() {
        name.push(".");
        name.push(extension);
    }

    let path = builder.path.with_file_name(name);

    // Try to create a new sink file
    let _ = OpenOptions::new()
        .append(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| {
            anyhow!(
                "unable to create avro ocf sink file {} : {}",
                path.display(),
                e
            )
        })?;
    Ok(SinkConnector::AvroOcf(AvroOcfSinkConnector {
        path,
        value_desc: builder.value_desc,
    }))
}
