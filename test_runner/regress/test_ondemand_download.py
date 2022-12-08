# It's possible to run any regular test with the local fs remote storage via
# env ZENITH_PAGESERVER_OVERRIDES="remote_storage={local_path='/tmp/neon_zzz/'}" poetry ......

import os
import shutil
from pathlib import Path

import pytest
from fixtures.log_helper import log
from fixtures.neon_fixtures import (
    NeonEnvBuilder,
    RemoteStorageKind,
    assert_tenant_status,
    available_remote_storages,
    wait_for_last_record_lsn,
    wait_for_sk_commit_lsn_to_reach_remote_storage,
    wait_for_upload,
    wait_until,
)
from fixtures.types import Lsn
from fixtures.utils import query_scalar


def get_num_downloaded_layers(client, tenant_id, timeline_id):
    value = client.get_metric_value(
        f'pageserver_remote_operation_seconds_count{{file_kind="layer",op_kind="download",status="success",tenant_id="{tenant_id}",timeline_id="{timeline_id}"}}'
    )
    if value is None:
        return 0
    return int(value)


#
# If you have a large relation, check that the pageserver downloads parts of it as
# require by queries.
#
@pytest.mark.parametrize("remote_storage_kind", available_remote_storages())
def test_ondemand_download_large_rel(
    neon_env_builder: NeonEnvBuilder,
    remote_storage_kind: RemoteStorageKind,
):
    neon_env_builder.enable_remote_storage(
        remote_storage_kind=remote_storage_kind,
        test_name="test_ondemand_download_large_rel",
    )

    ##### First start, insert secret data and upload it to the remote storage
    env = neon_env_builder.init_start()

    # Override defaults, to create more layers
    tenant, _ = env.neon_cli.create_tenant(
        conf={
            # disable background GC
            "gc_period": "10 m",
            "gc_horizon": f"{10 * 1024 ** 3}",  # 10 GB
            # small checkpoint distance to create more delta layer files
            "checkpoint_distance": f"{10 * 1024 ** 2}",  # 10 MB
            "compaction_threshold": "3",
            "compaction_target_size": f"{10 * 1024 ** 2}",  # 10 MB
        }
    )
    env.initial_tenant = tenant

    pg = env.postgres.create_start("main")

    client = env.pageserver.http_client()

    tenant_id = pg.safe_psql("show neon.tenant_id")[0][0]
    timeline_id = pg.safe_psql("show neon.timeline_id")[0][0]

    # We want to make sure that the data is large enough that the keyspace is partitioned.
    num_rows = 1000000

    with pg.cursor() as cur:
        # data loading may take a while, so increase statement timeout
        cur.execute("SET statement_timeout='300s'")
        cur.execute(
            f"""CREATE TABLE tbl AS SELECT g as id, 'long string to consume some space' || g
        from generate_series(1,{num_rows}) g"""
        )
        cur.execute("CREATE INDEX ON tbl (id)")
        cur.execute("VACUUM tbl")

        current_lsn = Lsn(query_scalar(cur, "SELECT pg_current_wal_flush_lsn()"))

    # wait until pageserver receives that data
    wait_for_last_record_lsn(client, tenant_id, timeline_id, current_lsn)

    # run checkpoint manually to be sure that data landed in remote storage
    client.timeline_checkpoint(tenant_id, timeline_id)

    # wait until pageserver successfully uploaded a checkpoint to remote storage
    wait_for_upload(client, tenant_id, timeline_id, current_lsn)
    log.info("uploads have finished")

    ##### Stop the first pageserver instance, erase all its data
    pg.stop()
    env.pageserver.stop()

    dir_to_clear = Path(env.repo_dir) / "tenants"
    shutil.rmtree(dir_to_clear)
    os.mkdir(dir_to_clear)

    ##### Second start, restore the data and ensure it's the same
    env.pageserver.start()

    client.tenant_attach(tenant_id)

    pg.start()
    before_downloads = get_num_downloaded_layers(client, tenant_id, timeline_id)

    # Probe in the middle of the table. There's a high chance that the beginning
    # and end of the table was stored together in the same layer files with data
    # from other tables, and with the entry that stores the size of the
    # relation, so they are likely already downloaded. But the middle of the
    # table should not have been needed by anything yet.
    with pg.cursor() as cur:
        assert query_scalar(cur, "select count(*) from tbl where id = 500000") == 1

    after_downloads = get_num_downloaded_layers(client, tenant_id, timeline_id)
    log.info(f"layers downloaded before {before_downloads} and after {after_downloads}")
    assert after_downloads > before_downloads


#
# If you have a relation with a long history of updates,the pageserver downloads the layer
# files containing the history as needed by timetravel queries.
#
@pytest.mark.parametrize("remote_storage_kind", available_remote_storages())
def test_ondemand_download_timetravel(
    neon_env_builder: NeonEnvBuilder,
    remote_storage_kind: RemoteStorageKind,
):
    neon_env_builder.enable_remote_storage(
        remote_storage_kind=remote_storage_kind,
        test_name="test_ondemand_download_timetravel",
    )

    ##### First start, insert data and upload it to the remote storage
    env = neon_env_builder.init_start()

    # Override defaults, to create more layers
    tenant, _ = env.neon_cli.create_tenant(
        conf={
            # Disable background GC & compaction
            # We don't want GC, that would break the assertion about num downloads.
            # We don't want background compaction, we force a compaction every time we do explicit checkpoint.
            "gc_period": "0s",
            "compaction_period": "0s",
            # small checkpoint distance to create more delta layer files
            "checkpoint_distance": f"{1 * 1024 ** 2}",  # 1 MB
            "compaction_threshold": "1",
            "image_creation_threshold": "1",
            "compaction_target_size": f"{1 * 1024 ** 2}",  # 1 MB
        }
    )
    env.initial_tenant = tenant

    pg = env.postgres.create_start("main")

    client = env.pageserver.http_client()

    tenant_id = pg.safe_psql("show neon.tenant_id")[0][0]
    timeline_id = pg.safe_psql("show neon.timeline_id")[0][0]

    lsns = []

    table_len = 10000
    with pg.cursor() as cur:
        cur.execute(
            f"""
        CREATE TABLE testtab(id serial primary key, checkpoint_number int, data text);
        INSERT INTO testtab (checkpoint_number, data) SELECT 0, 'data' FROM generate_series(1, {table_len});
        """
        )
        current_lsn = Lsn(query_scalar(cur, "SELECT pg_current_wal_flush_lsn()"))
    # wait until pageserver receives that data
    wait_for_last_record_lsn(client, tenant_id, timeline_id, current_lsn)
    # run checkpoint manually to be sure that data landed in remote storage
    client.timeline_checkpoint(tenant_id, timeline_id)
    lsns.append((0, current_lsn))

    for checkpoint_number in range(1, 20):
        with pg.cursor() as cur:
            cur.execute(f"UPDATE testtab SET checkpoint_number = {checkpoint_number}")
            current_lsn = Lsn(query_scalar(cur, "SELECT pg_current_wal_flush_lsn()"))
        lsns.append((checkpoint_number, current_lsn))

        # wait until pageserver receives that data
        wait_for_last_record_lsn(client, tenant_id, timeline_id, current_lsn)

        # run checkpoint manually to be sure that data landed in remote storage
        client.timeline_checkpoint(tenant_id, timeline_id)

    # wait until pageserver successfully uploaded a checkpoint to remote storage
    wait_for_upload(client, tenant_id, timeline_id, current_lsn)
    log.info("uploads have finished")

    ##### Stop the first pageserver instance, erase all its data
    env.postgres.stop_all()

    wait_for_sk_commit_lsn_to_reach_remote_storage(
        tenant_id, timeline_id, env.safekeepers, env.pageserver
    )

    def get_physical_size():
        d = client.timeline_detail(tenant_id, timeline_id)
        return d["current_physical_size"]

    filled_size = get_physical_size()
    log.info(f"filled_size={filled_size}")

    env.pageserver.stop()

    dir_to_clear = Path(env.repo_dir) / "tenants"
    shutil.rmtree(dir_to_clear)
    os.mkdir(dir_to_clear)

    ##### Second start, restore the data and ensure it's the same
    env.pageserver.start()

    client.tenant_attach(tenant_id)

    wait_until(10, 0.2, lambda: assert_tenant_status(client, tenant_id, "Active"))

    num_layers_downloaded = [0]
    physical_size = [get_physical_size()]
    for (checkpoint_number, lsn) in lsns:
        pg_old = env.postgres.create_start(
            branch_name="main", node_name=f"test_old_lsn_{checkpoint_number}", lsn=lsn
        )
        with pg_old.cursor() as cur:
            # assert query_scalar(cur, f"select count(*) from testtab where checkpoint_number={checkpoint_number}") == 100000
            assert (
                query_scalar(
                    cur,
                    f"select count(*) from testtab where checkpoint_number<>{checkpoint_number}",
                )
                == 0
            )
            assert (
                query_scalar(
                    cur,
                    f"select count(*) from testtab where checkpoint_number={checkpoint_number}",
                )
                == table_len
            )

        after_downloads = get_num_downloaded_layers(client, tenant_id, timeline_id)
        num_layers_downloaded.append(after_downloads)
        log.info(f"num_layers_downloaded[-1]={num_layers_downloaded[-1]}")

        # Check that on each query, we need to download at least one more layer file. However in
        # practice, thanks to compaction and the fact that some requests need to download
        # more history, some points-in-time are covered by earlier downloads already. But
        # in broad strokes, as we query more points-in-time, more layers need to be downloaded.
        #
        # Do a fuzzy check on that, by checking that after each point-in-time, we have downloaded
        # more files than we had three iterations ago.
        log.info(f"layers downloaded after checkpoint {checkpoint_number}: {after_downloads}")
        if len(num_layers_downloaded) > 4:
            assert after_downloads > num_layers_downloaded[len(num_layers_downloaded) - 4]

        # Likewise, assert that the physical_size metric grows as layers are downloaded
        physical_size.append(get_physical_size())
        log.info(f"physical_size[-1]={physical_size[-1]}")
        if len(physical_size) > 4:
            assert physical_size[-1] > physical_size[len(physical_size) - 4]