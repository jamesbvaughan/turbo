use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use anyhow::{anyhow, Result};
use futures::{stream::FuturesUnordered, TryStreamExt};
use mime::TEXT_HTML_UTF_8;
use serde_json::Value as JsonValue;
use turbo_tasks::{
    primitives::{JsonValueVc, StringVc},
    spawn_blocking, CompletionVc, CompletionsVc, ValueToString,
};
use turbo_tasks_fs::{DiskFileSystemVc, File, FileContent, FileSystemPathVc};
use turbopack::ecmascript::EcmascriptModuleAssetVc;
use turbopack_core::{
    asset::{AssetContentVc, AssetVc, AssetsSetVc},
    chunk::{ChunkGroupVc, ChunkingContextVc},
};
use turbopack_ecmascript::chunk::EcmascriptChunkPlaceablesVc;

use self::{
    bootstrap::NodeJsBootstrapAsset,
    pool::{NodeJsPool, NodeJsPoolVc},
};
use crate::nodejs::issue::RenderingIssue;

pub mod bootstrap;
pub(crate) mod issue;
pub mod pool;

#[turbo_tasks::function]
async fn emit(
    intermediate_asset: AssetVc,
    intermediate_output_path: FileSystemPathVc,
) -> Result<CompletionVc> {
    Ok(CompletionsVc::cell(
        internal_assets(intermediate_asset, intermediate_output_path)
            .await?
            .iter()
            .map(|a| a.content().write(a.path()))
            .collect(),
    )
    .all())
}

/// List of the all assets of the "internal" subgraph and a list of boundary
/// assets that are not considered "internal" ("external")
#[turbo_tasks::value]
pub struct SeparatedAssets {
    pub internal_assets: AssetsSetVc,
    pub external_asset_entrypoints: AssetsSetVc,
}

/// Extracts the subgraph of "internal" assets (assets within the passes
/// directory). Also lists all boundary assets that are not part of the
/// "internal" subgraph.
#[turbo_tasks::function]
async fn internal_assets(
    intermediate_asset: AssetVc,
    intermediate_output_path: FileSystemPathVc,
) -> Result<AssetsSetVc> {
    Ok(
        separate_assets(intermediate_asset, intermediate_output_path)
            .await?
            .internal_assets,
    )
}

/// Returns a set of "external" assets on the boundary of the "internal"
/// subgraph
#[turbo_tasks::function]
pub async fn external_asset_entrypoints(
    module: EcmascriptModuleAssetVc,
    runtime_entries: EcmascriptChunkPlaceablesVc,
    chunking_context: ChunkingContextVc,
    intermediate_output_path: FileSystemPathVc,
) -> Result<AssetsSetVc> {
    Ok(separate_assets(
        get_intermediate_asset(
            module,
            runtime_entries,
            chunking_context,
            intermediate_output_path,
        ),
        intermediate_output_path,
    )
    .await?
    .external_asset_entrypoints)
}

/// Splits the asset graph into "internal" assets and boundaries to "external"
/// assets.
#[turbo_tasks::function]
async fn separate_assets(
    intermediate_asset: AssetVc,
    intermediate_output_path: FileSystemPathVc,
) -> Result<SeparatedAssetsVc> {
    enum Type {
        Internal(AssetVc, Vec<AssetVc>),
        External(AssetVc),
    }
    let intermediate_output_path = intermediate_output_path.await?;
    let mut queue = FuturesUnordered::new();
    let process_asset = |asset: AssetVc| {
        let intermediate_output_path = &intermediate_output_path;
        async move {
            // Assets within the output directory are considered as "internal" and all
            // others as "external". We follow references on "internal" assets, but do not
            // look into references of "external" assets, since there are no "internal"
            // assets behind "externals"
            if asset.path().await?.is_inside(intermediate_output_path) {
                let mut assets = Vec::new();
                for reference in asset.references().await?.iter() {
                    for asset in reference.resolve_reference().primary_assets().await?.iter() {
                        assets.push(*asset);
                    }
                }
                Ok::<_, anyhow::Error>(Type::Internal(asset, assets))
            } else {
                Ok(Type::External(asset))
            }
        }
    };
    queue.push(process_asset(intermediate_asset));
    let mut processed = HashSet::new();
    let mut internal_assets = HashSet::new();
    let mut external_asset_entrypoints = HashSet::new();
    while let Some(item) = queue.try_next().await? {
        match item {
            Type::Internal(asset, assets) => {
                internal_assets.insert(asset);
                for asset in assets {
                    if processed.insert(asset) {
                        queue.push(process_asset(asset));
                    }
                }
            }
            Type::External(asset) => {
                external_asset_entrypoints.insert(asset);
            }
        }
    }
    Ok(SeparatedAssets {
        internal_assets: AssetsSetVc::cell(internal_assets),
        external_asset_entrypoints: AssetsSetVc::cell(external_asset_entrypoints),
    }
    .cell())
}

/// Creates a node.js renderer pool for an entrypoint.
#[turbo_tasks::function]
pub async fn get_renderer_pool(
    intermediate_asset: AssetVc,
    intermediate_output_path: FileSystemPathVc,
) -> Result<NodeJsPoolVc> {
    emit(intermediate_asset, intermediate_output_path).await?;
    let output = intermediate_output_path.await?;
    if let Some(disk) = DiskFileSystemVc::resolve_from(output.fs).await? {
        let dir = PathBuf::from(&disk.await?.root).join(&output.path);
        let entrypoint = dir.join("index.js");
        let pool = NodeJsPool::new(dir, entrypoint, HashMap::new(), 4);
        Ok(pool.cell())
    } else {
        Err(anyhow!("can only render from a disk filesystem"))
    }
}

/// Converts a module graph into node.js executable assets
#[turbo_tasks::function]
async fn get_intermediate_asset(
    entry_module: EcmascriptModuleAssetVc,
    runtime_entries: EcmascriptChunkPlaceablesVc,
    chunking_context: ChunkingContextVc,
    intermediate_output_path: FileSystemPathVc,
) -> Result<AssetVc> {
    let chunk = entry_module.as_evaluated_chunk(chunking_context.into(), Some(runtime_entries));
    let chunk_group = ChunkGroupVc::from_chunk(chunk);
    Ok(NodeJsBootstrapAsset {
        path: intermediate_output_path.join("index.js"),
        chunk_group,
    }
    .cell()
    .into())
}

/// Renders a module as static HTML in a node.js process.
#[turbo_tasks::function]
pub async fn render_static(
    path: FileSystemPathVc,
    module: EcmascriptModuleAssetVc,
    runtime_entries: EcmascriptChunkPlaceablesVc,
    chunking_context: ChunkingContextVc,
    intermediate_output_path: FileSystemPathVc,
    data: JsonValueVc,
) -> Result<AssetContentVc> {
    fn into_result(content: String) -> Result<AssetContentVc> {
        Ok(
            FileContent::Content(File::from_source(content).with_content_type(TEXT_HTML_UTF_8))
                .into(),
        )
    }
    let renderer_pool = get_renderer_pool(
        get_intermediate_asset(
            module,
            runtime_entries,
            chunking_context,
            intermediate_output_path,
        ),
        intermediate_output_path,
    );
    let pool = renderer_pool.await?;
    let mut op = pool.run(data.to_string().await?.as_bytes()).await?;
    let lines = spawn_blocking(move || {
        let lines = op.read_lines()?;
        drop(op);
        Ok::<_, anyhow::Error>(lines)
    })
    .await?;
    let issue = if let Some(last_line) = lines.last() {
        if let Some(data) = last_line.strip_prefix("RESULT=") {
            let data: JsonValue = serde_json::from_str(data)?;
            if let Some(s) = data.as_str() {
                return into_result(s.to_string());
            } else {
                RenderingIssue {
                    context: path,
                    message: StringVc::cell(
                        "Result provided by Node.js rendering process was not a string".to_string(),
                    ),
                    logging: StringVc::cell(lines.join("\n")),
                }
            }
        } else if let Some(data) = last_line.strip_prefix("ERROR=") {
            let data: JsonValue = serde_json::from_str(data)?;
            if let Some(s) = data.as_str() {
                RenderingIssue {
                    context: path,
                    message: StringVc::cell(s.to_string()),
                    logging: StringVc::cell(lines[..lines.len() - 1].join("\n")),
                }
            } else {
                RenderingIssue {
                    context: path,
                    message: StringVc::cell(data.to_string()),
                    logging: StringVc::cell(lines[..lines.len() - 1].join("\n")),
                }
            }
        } else {
            RenderingIssue {
                context: path,
                message: StringVc::cell("No result provided by Node.js process".to_string()),
                logging: StringVc::cell(lines.join("\n")),
            }
        }
    } else {
        RenderingIssue {
            context: path,
            message: StringVc::cell("No content received from Node.js process.".to_string()),
            logging: StringVc::cell("".to_string()),
        }
    };

    // Show error page
    // TODO This need to include HMR handler to allow auto refresh
    let result = into_result(format!(
        "<h1>Error during \
         rendering</h1>\n<h2>Message</h2>\n<pre>{}</pre>\n<h2>Logs</h2>\n<pre>{}</pre>",
        issue.message.await?,
        issue.logging.await?
    ));

    // Emit an issue for error reporting
    issue.cell().as_issue().emit();

    result
}
