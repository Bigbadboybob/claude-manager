//! Operator-only enrollment and first-space handoff. Session processes and PTYs
//! are outside this transaction. Event bytes and IDs are never rewritten.
use super::*;
use std::os::unix::fs::PermissionsExt;

fn transaction_child(root: &Path, name: &str) -> Result<PathBuf> {
    if !name.starts_with("messaging-")
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
    {
        return Err(err("invalid_handoff", "Unsafe installation path"));
    }
    Ok(root.join(name))
}
/// Resume a prepared directory swap before reopening the store. Its marker is
/// fsynced first; backups are retained for explicit operator rollback/inspection.
pub(super) fn finish_install(root: &Path) -> Result<()> {
    let path = root.join("messaging-install.json");
    if !path.exists() {
        return Ok(());
    }
    let record = load(&path)?;
    let staged = transaction_child(root, &required(&record, "staged")?)?;
    let backup = transaction_child(root, &required(&record, "backup")?)?;
    let target = root.join("messages/main");
    let installed = target.join("space.json").exists()
        && load(&target.join("space.json"))?["installation_id"] == record["installation_id"];
    if !installed {
        if !staged.exists() {
            return Err(err(
                "invalid_handoff",
                "Prepared installation directory is missing",
            ));
        }
        if target.exists() {
            if backup.exists() {
                return Err(err(
                    "invalid_handoff",
                    "Destination changed during installation; inspect retained backup",
                ));
            }
            fs::rename(&target, &backup)?;
            File::open(root.join("messages"))?.sync_all()?;
            File::open(root)?.sync_all()?;
        }
        mkdir(target.parent().unwrap())?;
        fs::rename(&staged, &target)?;
        File::open(target.parent().unwrap())?.sync_all()?;
        File::open(root)?.sync_all()?;
    }
    atomic_replace(&root.join("messaging-sync.json"), &record["config"])?;
    fs::remove_file(&path)?;
    File::open(root)?.sync_all()?;
    Ok(())
}
impl Store {
    fn cm_root(&self) -> &Path {
        self.root.parent().unwrap().parent().unwrap()
    }
    pub fn messaging_frozen(&self) -> bool {
        self.cm_root().join("messaging-handoff.json").exists()
    }
    pub(super) fn ensure_messaging_writable(&self) -> Result<()> {
        if self.messaging_frozen() {
            return Err(err("handoff_in_progress","Messaging writes are paused for coordinator handoff; retain drafts and request IDs"));
        }
        Ok(())
    }
    fn empty_for_enrollment(&self) -> bool {
        self.names.is_empty()
            && self.conversations.is_empty()
            && self.personal.is_empty()
            && self.events.iter().all(|p| {
                p.event["actor"]["id"] == "system"
                    && matches!(
                        p.event["type"].as_str(),
                        Some(
                            "channel.create"
                                | "norms.update"
                                | "channel.membership.initialize"
                                | "membership.enrollment"
                        )
                    )
            })
    }
    fn write_install(
        &mut self,
        staged: &Path,
        config: &Value,
        installation_id: &str,
    ) -> Result<()> {
        let root = self.cm_root().to_owned();
        let record = json!({"installation_id":installation_id,"staged":staged.file_name().unwrap().to_string_lossy(),"backup":format!("messaging-backup-{installation_id}"),"config":config});
        atomic_replace(&root.join("messaging-install.json"), &record)?;
        finish_install(&root)?;
        let lock = self._lock.try_clone()?;
        let reopened = Self::open_locked(&root, lock)?;
        if let Some(reason) = &reopened.degraded {
            return Err(err(
                "store_read_only",
                format!("Installed store failed verification: {reason}"),
            ));
        }
        *self = reopened;
        Ok(())
    }
    /// The caller is authenticated as Operator before reaching this surface.
    pub fn sync_admin(&mut self, p: &Value, people: &[Person]) -> Result<Value> {
        if p["action"].as_str().unwrap_or("status") != "status" {
            if let Some(reason) = &self.degraded {
                return Err(err("store_read_only", reason.clone()));
            }
        }
        match p["action"].as_str().unwrap_or("status") {
            "status" => Ok(
                json!({"sync":self.sync_status(),"descriptor":self.space_descriptor(),"empty_for_enrollment":self.empty_for_enrollment(),"handoff":if self.messaging_frozen(){load(&self.cm_root().join("messaging-handoff.json"))?}else{Value::Null}}),
            ),
            "enable_hub" => {
                self.ensure_messaging_writable()?;
                self.shared_mutation_allowed()?;
                self.register_host(&self.daemon_id.clone(), true, true, people)?;
                self.retain_legacy_owner_reads()?;
                let config = json!({"version":1,"configured":true,"space_id":self.space_id,"coordinator_id":self.daemon_id});
                atomic_replace(&self.cm_root().join("messaging-sync.json"), &config)?;
                self.replication.enabled = true;
                Ok(json!({"status":"enabled","descriptor":self.space_descriptor()}))
            }
            "pair" | "rotate_peer" => self.pair_peer(p),
            "revoke" => {
                self.ensure_messaging_writable()?;
                self.shared_mutation_allowed()?;
                let host = required(p, "host_id")?;
                Uuid::parse_str(&host).map_err(|_| err("invalid_params", "Invalid host UUID"))?;
                let path = self
                    .cm_root()
                    .join("messaging-peers")
                    .join(format!("{host}.json"));
                let mut key = load(&path)?;
                key["active"] = json!(false);
                atomic_replace(&path, &key)?;
                let people = self
                    .replication
                    .people
                    .values()
                    .filter(|p| p.id.starts_with(&format!("agent:{host}:")))
                    .cloned()
                    .collect::<Vec<_>>();
                self.register_host(&host, false, key["owner_access"] == true, &people)?;
                Ok(json!({"status":"revoked","host_id":host}))
            }
            "join_space" => self.join_space(p),
            "prepare_handoff" => self.prepare_handoff(p, people),
            "install_seed" => self.install_seed(p),
            "activate_replica" => self.activate_replica(p),
            "abort_destination" => {
                let id = required(p, "handoff_id")?;
                Uuid::parse_str(&id).map_err(|_| err("invalid_params", "Invalid handoff UUID"))?;
                let meta = load(&self.root.join("space.json"))?;
                if meta["accepted_handoff"] == id {
                    return Err(err("handoff_installed","Destination is already authoritative; complete activation instead of aborting"));
                }
                atomic_replace(
                    &self
                        .cm_root()
                        .join("messaging-handoffs")
                        .join(format!("{id}.json")),
                    &json!({"status":"aborted","handoff_id":id,"target_id":self.daemon_id}),
                )?;
                Ok(json!({"status":"aborted","handoff_id":id,"target_id":self.daemon_id}))
            }
            "abort_source" => {
                let path = self.cm_root().join("messaging-handoff.json");
                let handoff = load(&path)?;
                let ack = &p["destination_ack"];
                if ack["status"] != "aborted"
                    || ack["handoff_id"] != handoff["handoff_id"]
                    || ack["target_id"] != handoff["target_id"]
                {
                    return Err(err(
                        "invalid_handoff",
                        "Resume needs the destination's durable abort acknowledgement",
                    ));
                }
                fs::remove_file(&path)?;
                File::open(self.cm_root())?.sync_all()?;
                Ok(json!({"status":"resumed","coordinator_id":self.daemon_id}))
            }
            _ => Err(err(
                "invalid_params",
                "Unknown messaging administration action",
            )),
        }
    }
    fn pair_peer(&mut self, p: &Value) -> Result<Value> {
        self.ensure_messaging_writable()?;
        self.shared_mutation_allowed()?;
        let host = required(p, "host_id")?;
        Uuid::parse_str(&host).map_err(|_| err("invalid_params", "Invalid host UUID"))?;
        if host == self.daemon_id {
            return Err(err("invalid_params", "A coordinator cannot pair to itself"));
        }
        let path = self
            .cm_root()
            .join("messaging-peers")
            .join(format!("{host}.json"));
        if path.exists() && p["action"] != "rotate_peer" {
            return Err(err(
                "already_paired",
                "Peer exists; use rotate_peer to replace its credential explicitly",
            ));
        }
        let token = format!("{}{}", uuid(), uuid());
        let token_path = self
            .cm_root()
            .join("messaging-pairing-out")
            .join(format!("{host}-{}.token", uuid()));
        write_file(&token_path, token.as_bytes(), false)?;
        atomic_replace(
            &path,
            &json!({"token_sha256":hash(token.as_bytes()),"active":true,"owner_access":p["owner_access"]==true}),
        )?;
        let people = self
            .replication
            .people
            .values()
            .filter(|p| p.id.starts_with(&format!("agent:{host}:")))
            .cloned()
            .collect::<Vec<_>>();
        self.register_host(&host, true, p["owner_access"] == true, &people)?;
        Ok(
            json!({"status":"paired","host_id":host,"token_file":token_path,"descriptor":self.space_descriptor(),"credential_scope":"messaging_only"}),
        )
    }
    fn replica_config(&self, p: &Value, descriptor: &Value) -> Result<Value> {
        let endpoint: crate::messaging::sync::Endpoint =
            serde_json::from_value(p["endpoint"].clone())?;
        let token = PathBuf::from(required(p, "token_file")?);
        if !token.is_absolute() || !token.is_file() {
            return Err(err(
                "invalid_config",
                "token_file must be an existing absolute path",
            ));
        }
        let permissions = fs::metadata(&token)?.permissions().mode();
        if permissions & 0o077 != 0 {
            return Err(err(
                "invalid_config",
                "Pairing token must be readable only by its owner (chmod 600)",
            ));
        }
        Ok(
            json!({"version":1,"configured":true,"coordinator_id":descriptor["coordinator_id"],"space_id":descriptor["space_id"],"endpoint":endpoint,"token_file":token}),
        )
    }
    fn join_space(&mut self, p: &Value) -> Result<Value> {
        self.ensure_messaging_writable()?;
        if !self.empty_for_enrollment() {
            return Err(err(
                "store_exists",
                "Cannot enroll a populated space; use an explicit handoff or a separate CM root",
            ));
        }
        let descriptor = &p["descriptor"];
        for key in ["space_id", "coordinator_id", "owner_identity_revision"] {
            Uuid::parse_str(&required(descriptor, key)?)
                .map_err(|_| err("invalid_space", "Invalid shared-space descriptor"))?;
        }
        if descriptor["coordinator_id"] == self.daemon_id {
            return Err(err("invalid_space", "Use enable_hub for this coordinator"));
        }
        let config = self.replica_config(p, descriptor)?;
        let install = uuid();
        let staged = self.cm_root().join(format!("messaging-stage-{install}"));
        mkdir(&staged)?;
        let mut meta = descriptor.clone();
        meta["protocol"] = json!(1);
        meta["replica_id"] = json!(self.daemon_id);
        meta["generation"] = json!(uuid());
        meta["enrollment_revision"] = json!(uuid());
        meta["installation_id"] = json!(install);
        atomic_replace(&staged.join("space.json"), &meta)?;
        self.write_install(&staged, &config, &install)?;
        Ok(json!({"status":"enrolled","sync":self.sync_status()}))
    }
    fn prepare_handoff(&mut self, p: &Value, people: &[Person]) -> Result<Value> {
        self.shared_mutation_allowed()?;
        if self.sync_enabled() {
            return Err(err(
                "invalid_handoff",
                "Initial handoff requires a standalone source without enrolled relay peers",
            ));
        }
        let target = required(p, "target_id")?;
        Uuid::parse_str(&target)
            .map_err(|_| err("invalid_params", "Invalid destination daemon UUID"))?;
        if target == self.daemon_id {
            return Err(err(
                "invalid_handoff",
                "Source and destination must be distinct daemons",
            ));
        }
        let output = PathBuf::from(required(p, "output")?);
        if !output.is_absolute() || output.starts_with(&self.root) {
            return Err(err(
                "invalid_params",
                "Use an absolute seed directory outside the messaging store",
            ));
        }
        let marker = self.cm_root().join("messaging-handoff.json");
        let handoff = if marker.exists() {
            let old = load(&marker)?;
            if old["target_id"] != target || old["output"] != json!(output) {
                return Err(err(
                    "handoff_in_progress",
                    "A different handoff is already prepared",
                ));
            }
            old
        } else {
            if output.exists() {
                return Err(err(
                    "destination_exists",
                    "Seed output must be a new directory",
                ));
            }
            self.register_host(&self.daemon_id.clone(), true, true, people)?;
            self.retain_legacy_owner_reads()?;
            let record = json!({"handoff_id":uuid(),"source_id":self.daemon_id,"target_id":target,"space_id":self.space_id,"source_position":self.position,"output":output});
            atomic_replace(&marker, &record)?;
            record
        };
        mkdir(&output)?;
        let mut files = BTreeMap::<String, String>::new();
        for entry in fs::read_dir(self.journal_dir())? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let rel = path
                .strip_prefix(&self.root)
                .unwrap()
                .to_string_lossy()
                .to_string();
            let bytes = fs::read(&path)?;
            write_file(&output.join(&rel), &bytes, true)?;
            files.insert(rel, hash(&bytes));
        }
        for event in &self.events {
            let path = self.event_path(&event.event)?;
            let rel = path
                .strip_prefix(&self.root)
                .unwrap()
                .to_string_lossy()
                .to_string();
            let bytes = fs::read(path)?;
            write_file(&output.join(&rel), &bytes, true)?;
            files.insert(rel, hash(&bytes));
        }
        // Owner moves to the hub executor. Session-owned private state remains
        // on the source, keyed by that host's immutable participant IDs.
        for path in [
            self.personal_path("owner"),
            self.root
                .join("_state")
                .join(format!("{}.json", hash(b"owner"))),
        ] {
            if !path.exists() {
                continue;
            }
            let rel = path
                .strip_prefix(&self.root)
                .unwrap()
                .to_string_lossy()
                .to_string();
            let bytes = fs::read(path)?;
            write_file(&output.join(&rel), &bytes, true)?;
            files.insert(rel, hash(&bytes));
        }
        let descriptor = load(&self.root.join("space.json"))?;
        let manifest =
            json!({"protocol":1,"handoff":handoff,"descriptor":descriptor,"files":files});
        atomic_replace(&output.join("SEED.json"), &manifest)?;
        Ok(
            json!({"status":"prepared","handoff":handoff,"seed_sha256":hash(&serde_json::to_vec(&manifest)?),"note":"Messaging writes paused; sessions continue running"}),
        )
    }
    fn install_seed(&mut self, p: &Value) -> Result<Value> {
        let seed = PathBuf::from(required(p, "seed")?);
        let manifest = load(&seed.join("SEED.json"))?;
        let handoff = &manifest["handoff"];
        let id = required(handoff, "handoff_id")?;
        Uuid::parse_str(&id).map_err(|_| err("invalid_handoff", "Invalid handoff UUID"))?;
        let seed_digest = hash(&serde_json::to_vec(&manifest)?);
        if p["seed_sha256"] != seed_digest {
            return Err(err(
                "invalid_handoff",
                "Seed checksum differs from the source's prepared manifest",
            ));
        }
        let ack = json!({"status":"installed","handoff_id":id,"target_id":self.daemon_id,"space_id":handoff["space_id"],"seed_sha256":seed_digest});
        let old = load(&self.root.join("space.json"))?;
        if old["accepted_handoff"] == id {
            if old["seed_sha256"] != seed_digest {
                return Err(err(
                    "invalid_handoff",
                    "Installed handoff has a different seed checksum",
                ));
            }
            return Ok(ack);
        }
        if !self.empty_for_enrollment() {
            return Err(err(
                "store_exists",
                "Destination has messaging history; refusing to overwrite it",
            ));
        }
        if handoff["target_id"] != self.daemon_id
            || manifest["descriptor"]["replica_id"] != handoff["source_id"]
        {
            return Err(err(
                "invalid_handoff",
                "Seed is for a different source/destination",
            ));
        }
        if self
            .cm_root()
            .join("messaging-handoffs")
            .join(format!("{id}.json"))
            .exists()
        {
            return Err(err("handoff_aborted", "This handoff was durably aborted"));
        }
        let install = uuid();
        let staged = self.cm_root().join(format!("messaging-stage-{install}"));
        mkdir(&staged)?;
        let mut meta = manifest["descriptor"].clone();
        let generation = uuid();
        let mut lineage: BTreeSet<String> = serde_json::from_value(
            meta.get("coordinator_lineage")
                .cloned()
                .unwrap_or(json!([])),
        )?;
        lineage.insert(required(&meta, "coordinator_id")?);
        meta["coordinator_lineage"] = json!(lineage);
        meta["coordinator_id"] = json!(self.daemon_id);
        meta["replica_id"] = json!(self.daemon_id);
        meta["generation"] = json!(generation);
        meta["installation_id"] = json!(install);
        meta["accepted_handoff"] = json!(id);
        meta["seed_sha256"] = json!(seed_digest);
        let files = manifest["files"]
            .as_object()
            .ok_or_else(|| err("invalid_handoff", "Seed manifest needs file checksums"))?;
        for (relative, expected) in files {
            let relative_path = Path::new(relative);
            if relative_path.is_absolute()
                || relative_path
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                return Err(err("invalid_handoff", "Unsafe seed member path"));
            }
            let source = seed.join(relative);
            if !fs::symlink_metadata(&source)?.file_type().is_file() {
                return Err(err("invalid_handoff", "Seed members must be regular files"));
            }
            let bytes = fs::read(&source)?;
            if expected != &hash(&bytes) {
                return Err(err(
                    "invalid_handoff",
                    format!("Checksum mismatch for {relative}"),
                ));
            }
            if relative.starts_with("_journal/") {
                let mut journal: Value = serde_json::from_slice(&bytes)?;
                journal["replica_id"] = json!(self.daemon_id);
                journal["generation"] = json!(generation);
                let position = required(&journal, "position")?
                    .parse::<u64>()
                    .map_err(|_| err("invalid_handoff", "Invalid journal position"))?;
                write_file(
                    &staged
                        .join("_journal")
                        .join(&generation)
                        .join(format!("{position:020}.json")),
                    &serde_json::to_vec_pretty(&journal)?,
                    false,
                )?;
            } else {
                write_file(&staged.join(relative), &bytes, false)?;
            }
        }
        atomic_replace(&staged.join("space.json"), &meta)?;
        // Verify the transformed journal and all original bodies with the real
        // reducer in a disposable sibling root before changing the destination.
        let verify = tempfile::tempdir_in(self.cm_root())?;
        write_file(
            &verify.path().join("daemon-id"),
            self.daemon_id.as_bytes(),
            false,
        )?;
        mkdir(&verify.path().join("messages"))?;
        fs::rename(&staged, verify.path().join("messages/main"))?;
        let candidate = Self::open(verify.path())?;
        if let Some(reason) = &candidate.degraded {
            return Err(err(
                "invalid_handoff",
                format!("Seed validation failed: {reason}"),
            ));
        }
        drop(candidate);
        fs::rename(verify.path().join("messages/main"), &staged)?;
        let config = json!({"version":1,"configured":true,"space_id":meta["space_id"],"coordinator_id":self.daemon_id});
        self.write_install(&staged, &config, &install)?;
        Ok(ack)
    }
    fn activate_replica(&mut self, p: &Value) -> Result<Value> {
        let marker = self.cm_root().join("messaging-handoff.json");
        let ack = &p["destination_ack"];
        if !marker.exists() {
            let meta = load(&self.root.join("space.json"))?;
            if ack["handoff_id"].is_string()
                && meta["activated_handoff"] == ack["handoff_id"]
                && meta["activated_seed_sha256"] == ack["seed_sha256"]
                && meta["coordinator_id"] == ack["target_id"]
            {
                return Ok(json!({"status":"activated","sync":self.sync_status()}));
            }
            return Err(err("invalid_handoff", "No matching prepared handoff"));
        }
        let handoff = load(&marker)?;
        let seed = load(&PathBuf::from(required(&handoff, "output")?).join("SEED.json"))?;
        if ack["status"] != "installed"
            || ack["handoff_id"] != handoff["handoff_id"]
            || ack["target_id"] != handoff["target_id"]
            || ack["seed_sha256"] != hash(&serde_json::to_vec(&seed)?)
        {
            return Err(err(
                "invalid_handoff",
                "Activation needs the destination's matching installed acknowledgement",
            ));
        }
        let mut descriptor = self.space_descriptor();
        descriptor["coordinator_id"] = handoff["target_id"].clone();
        let config = self.replica_config(p, &descriptor)?;
        let mut meta = load(&self.root.join("space.json"))?;
        self.replication
            .coordinator_lineage
            .insert(self.daemon_id.clone());
        meta["coordinator_lineage"] = json!(self.replication.coordinator_lineage);
        meta["coordinator_id"] = handoff["target_id"].clone();
        meta["activated_handoff"] = handoff["handoff_id"].clone();
        meta["activated_seed_sha256"] = ack["seed_sha256"].clone();
        // Config is installed while writes remain fenced. A crash at either
        // replacement resumes with the same handoff marker and original IDs.
        atomic_replace(&self.cm_root().join("messaging-sync.json"), &config)?;
        atomic_replace(&self.root.join("space.json"), &meta)?;
        self.replication.coordinator_id = required(&handoff, "target_id")?;
        self.replication.enabled = true;
        fs::remove_file(&marker)?;
        File::open(self.cm_root())?.sync_all()?;
        Ok(json!({"status":"activated","sync":self.sync_status()}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn messaging_install_recovers_at_each_directory_swap_boundary() {
        for phase in 0..3 {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("destination");
            let old = Store::open(&root).unwrap();
            let old_space = old.space_id.clone();
            let daemon = old.daemon_id.clone();
            drop(old);
            let scratch = tmp.path().join("scratch");
            mkdir(&scratch).unwrap();
            write_file(&scratch.join("daemon-id"), daemon.as_bytes(), false).unwrap();
            let staged_store = Store::open(&scratch).unwrap();
            let space = staged_store.space_id.clone();
            let installation_id = uuid();
            let mut meta = load(&staged_store.root.join("space.json")).unwrap();
            meta["installation_id"] = json!(installation_id);
            atomic_replace(&staged_store.root.join("space.json"), &meta).unwrap();
            drop(staged_store);
            let staged = root.join(format!("messaging-stage-{installation_id}"));
            let backup = root.join(format!("messaging-backup-{installation_id}"));
            fs::rename(scratch.join("messages/main"), &staged).unwrap();
            let config =
                json!({"version":1,"configured":true,"space_id":space,"coordinator_id":daemon});
            atomic_replace(&root.join("messaging-install.json"), &json!({"installation_id":installation_id,
                "staged":staged.file_name().unwrap().to_str().unwrap(),"backup":backup.file_name().unwrap().to_str().unwrap(),"config":config})).unwrap();
            if phase >= 1 {
                fs::rename(root.join("messages/main"), &backup).unwrap();
            }
            if phase >= 2 {
                fs::rename(&staged, root.join("messages/main")).unwrap();
            }
            let recovered = Store::open(&root).unwrap();
            assert!(recovered.degraded.is_none(), "{:?}", recovered.degraded);
            assert_eq!(recovered.space_id, space);
            assert_eq!(
                load(&backup.join("space.json")).unwrap()["space_id"],
                old_space
            );
            assert_eq!(load(&root.join("messaging-sync.json")).unwrap(), config);
            assert!(!root.join("messaging-install.json").exists());
        }
    }
    #[test]
    fn invalid_seed_never_replaces_destination_and_owner_legacy_reads_transfer() {
        let tmp = tempfile::tempdir().unwrap();
        let mut source = Store::open(&tmp.path().join("source")).unwrap();
        let mut destination = Store::open(&tmp.path().join("destination")).unwrap();
        let actor = source.participant_id("writer");
        let people = [Person {
            id: actor.clone(),
            name: "Writer".into(),
            session_uid: "writer".into(),
            task: None,
            present: true,
            kind: "agent".into(),
        }];
        let sent = source.send(&actor, "writer", "agent", &json!({"channel":"general","name":"Writer","body":"Already read","request_id":"read"}), &people).unwrap();
        let id = sent["event_id"].as_str().unwrap().to_owned();
        source
            .merge_read_ids("owner", BTreeSet::from([id.clone()]))
            .unwrap();
        let seed = tmp.path().join("seed");
        let prepare = source.sync_admin(&json!({"action":"prepare_handoff","target_id":destination.daemon_id,"output":seed}), &people).unwrap();
        let original = destination.space_id.clone();
        let install =
            json!({"action":"install_seed","seed":seed,"seed_sha256":prepare["seed_sha256"]});
        let manifest = load(&seed.join("SEED.json")).unwrap();
        let relative = manifest["files"]
            .as_object()
            .unwrap()
            .keys()
            .next()
            .unwrap();
        let path = seed.join(relative);
        let bytes = fs::read(&path).unwrap();
        fs::write(&path, b"broken").unwrap();
        assert_eq!(
            destination.sync_admin(&install, &[]).unwrap_err().code,
            "invalid_handoff"
        );
        assert_eq!(destination.space_id, original);
        fs::write(path, bytes).unwrap();
        destination.sync_admin(&install, &[]).unwrap();
        destination.load_read("owner").unwrap();
        assert!(destination.reads["owner"].ids.contains(&id));
        assert!(destination
            .events
            .iter()
            .any(|e| e.event["type"] == "read.ack"
                && e.event["data"]["ids"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(id))));
    }
    #[test]
    fn messaging_handoff_preserves_ids_receipts_and_can_resume_both_endpoints() {
        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("source");
        let hub_root = tmp.path().join("hub");
        let mut source = Store::open(&source_root).unwrap();
        let mut hub = Store::open(&hub_root).unwrap();
        let actor = source.participant_id("builder");
        let person = Person {
            id: actor.clone(),
            name: "Builder".into(),
            session_uid: "builder".into(),
            task: None,
            present: true,
            kind: "agent".into(),
        };
        let request = json!({"channel":"general","name":"Builder","body":"Retained history","request_id":"original"});
        let sent = source
            .send(
                &actor,
                "builder",
                "agent",
                &request,
                std::slice::from_ref(&person),
            )
            .unwrap();
        let id = sent["event_id"].as_str().unwrap();
        let wire = source.wire_event(id).unwrap();
        let space = source.space_id.clone();
        let output = tmp.path().join("seed");
        let prepare = json!({"action":"prepare_handoff","target_id":hub.daemon_id,"output":output});
        let seed = source
            .sync_admin(&prepare, std::slice::from_ref(&person))
            .unwrap();
        assert_eq!(
            source
                .acknowledge(
                    "owner",
                    &json!({"actor":"owner","space_id":space,"ids":[id]})
                )
                .unwrap_err()
                .code,
            "handoff_in_progress"
        );
        assert_eq!(
            source
                .send(
                    &actor,
                    "builder",
                    "agent",
                    &json!({"channel":"general","body":"Blocked","request_id":"blocked"}),
                    &[]
                )
                .unwrap_err()
                .code,
            "handoff_in_progress"
        );
        drop(source);
        source = Store::open(&source_root).unwrap();
        assert!(source.messaging_frozen());
        assert!(source.degraded.is_none(), "{:?}", source.degraded);
        assert_eq!(
            source.sync_admin(&prepare, &[]).unwrap()["seed_sha256"],
            seed["seed_sha256"]
        );
        let install =
            json!({"action":"install_seed","seed":output,"seed_sha256":seed["seed_sha256"]});
        let ack = hub.sync_admin(&install, &[]).unwrap();
        assert_eq!(hub.sync_admin(&install, &[]).unwrap(), ack);
        assert_eq!(hub.space_id, space);
        assert_eq!(hub.wire_event(id).unwrap()["event"], wire["event"]);
        assert_eq!(hub.wire_event(id).unwrap()["receipt"], wire["receipt"]);
        let paired = hub
            .sync_admin(
                &json!({"action":"pair","host_id":source.daemon_id,"owner_access":true}),
                &[],
            )
            .unwrap();
        let token = source_root.join("pair-token");
        write_file(
            &token,
            &fs::read(paired["token_file"].as_str().unwrap()).unwrap(),
            false,
        )
        .unwrap();
        source.sync_admin(&json!({"action":"activate_replica","destination_ack":ack,"endpoint":{"kind":"unix","path":hub_root.join("messaging-sync.sock")},"token_file":token}),&[]).unwrap();
        assert!(!source.is_coordinator());
        assert!(!source.messaging_frozen());
        assert_eq!(
            source
                .send(
                    &actor,
                    "builder",
                    "agent",
                    &request,
                    std::slice::from_ref(&person)
                )
                .unwrap()["event_id"],
            id
        );
        let new = source
            .send(
                &actor,
                "builder",
                "agent",
                &json!({"channel":"general","body":"After handoff","request_id":"after"}),
                std::slice::from_ref(&person),
            )
            .unwrap();
        // Refresh enrollment metadata just as the authenticated stream does.
        let high = hub.position;
        let mut cursor = 0;
        loop {
            let page = hub
                .export_page(&source.daemon_id, &BTreeSet::new(), cursor, high)
                .unwrap();
            for wire in page["items"].as_array().unwrap() {
                source.ingest_replica(wire).unwrap();
            }
            cursor = page["cursor"].as_u64().unwrap();
            if page["complete"] == true {
                break;
            }
        }
        let receipt = hub
            .accept_upload(
                &source.daemon_id,
                &source
                    .wire_event(new["event_id"].as_str().unwrap())
                    .unwrap(),
            )
            .unwrap();
        source.record_receipt(&receipt).unwrap();
        drop(hub);
        hub = Store::open(&hub_root).unwrap();
        assert!(hub.degraded.is_none(), "{:?}", hub.degraded);
        drop(source);
        source = Store::open(&source_root).unwrap();
        assert!(source.degraded.is_none(), "{:?}", source.degraded);
    }
    #[test]
    fn messaging_handoff_abort_requires_destination_tombstone_and_prevents_late_install() {
        let tmp = tempfile::tempdir().unwrap();
        let mut source = Store::open(&tmp.path().join("source")).unwrap();
        let mut hub = Store::open(&tmp.path().join("hub")).unwrap();
        let output = tmp.path().join("seed");
        let seed = source
            .sync_admin(
                &json!({"action":"prepare_handoff","target_id":hub.daemon_id,"output":output}),
                &[],
            )
            .unwrap();
        assert_eq!(
            source
                .sync_admin(&json!({"action":"abort_source","destination_ack":{}}), &[])
                .unwrap_err()
                .code,
            "invalid_handoff"
        );
        let ack = hub
            .sync_admin(
                &json!({"action":"abort_destination","handoff_id":seed["handoff"]["handoff_id"]}),
                &[],
            )
            .unwrap();
        source
            .sync_admin(&json!({"action":"abort_source","destination_ack":ack}), &[])
            .unwrap();
        assert!(!source.messaging_frozen());
        assert_eq!(
            hub.sync_admin(
                &json!({"action":"install_seed","seed":output,"seed_sha256":seed["seed_sha256"]}),
                &[]
            )
            .unwrap_err()
            .code,
            "handoff_aborted"
        );
    }
}
