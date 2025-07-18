<!-- Copyright 2023 OpenObserve Inc.

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU Affero General Public License as published by
the Free Software Foundation, either version 3 of the License, or
(at your option) any later version.

This program is distributed in the hope that it will be useful
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
GNU Affero General Public License for more details.

You should have received a copy of the GNU Affero General Public License
along with this program.  If not, see <http://www.gnu.org/licenses/>.
-->

<template>
  <div v-if="currentRouteName === 'pipelines'">
    <div
      :class="
        store.state.theme === 'dark'
          ? 'dark-mode dark-theme'
          : 'light-theme light-mode'
      "
      class="full-wdith pipeline-list-table"
    >
      <q-table
        data-test="pipeline-list-table"
        ref="qTableRef"
        :rows="filteredPipelines"
        :columns="columns"
        row-key="name"
        :pagination="pagination"
        :filter="filterQuery"
        :filter-method="filterData"
        style="width: 100%"
        dense
        selection="multiple"
        v-model:selected="selectedPipelines"
      >
        <template v-slot:body="props">
          <q-tr
            :data-test="`pipeline-list-table-${props.row.pipeline_id}-row`"
            :props="props"
            style="cursor: pointer"
            @click="triggerExpand(props)"
          >
            <q-td auto-width>
              <q-checkbox
                v-model="props.selected"
                color="secondary"
                size="sm"
                @click.stop
              />
            </q-td>
            <q-td v-if="activeTab == 'scheduled'" auto-width>
              <q-btn
                dense
                flat
                size="xs"
                :icon="
                  expandedRow != props.row.pipeline_id
                    ? 'expand_more'
                    : 'expand_less'
                "
              />
            </q-td>
            <q-td v-for="col in filterColumns()" :key="col.name" :props="props">
              <template v-if="col.name !== 'actions'">
                {{ props.row[col.field] }}
              </template>
              <template v-else>
                <!-- Actions Buttons -->
                <q-btn
                  :data-test="`pipeline-list-${props.row.name}-pause-start-alert`"
                  :icon="props.row.enabled ? outlinedPause : outlinedPlayArrow"
                  class="q-ml-xs material-symbols-outlined"
                  padding="sm"
                  unelevated
                  size="sm"
                  :color="props.row.enabled ? 'negative' : 'positive'"
                  round
                  flat
                  :title="
                    props.row.enabled ? t('alerts.pause') : t('alerts.start')
                  "
                  @click.stop="toggleAlertState(props.row)"
                />
                <q-btn
                  :data-test="`pipeline-list-${props.row.name}-update-pipeline`"
                  icon="edit"
                  class="q-ml-xs"
                  padding="sm"
                  unelevated
                  size="sm"
                  round
                  flat
                  :title="t('pipeline.edit')"
                  @click.stop="editPipeline(props.row)"
                ></q-btn>
                <q-btn
                  :data-test="`pipeline-list-${props.row.name}-export-pipeline`"
                  icon="download"
                  class="q-ml-xs"
                  padding="sm"
                  unelevated
                  size="sm"
                  round
                  flat
                  :title="t('pipeline.export')"
                  @click.stop="exportPipeline(props.row)"
                ></q-btn>
                <q-btn
                  :data-test="`pipeline-list-${props.row.name}-delete-pipeline`"
                  :icon="outlinedDelete"
                  class="q-ml-xs"
                  padding="sm"
                  unelevated
                  size="sm"
                  round
                  flat
                  :title="t('pipeline.delete')"
                  @click.stop="openDeleteDialog(props.row)"
                ></q-btn>
                <q-btn
                  :data-test="`pipeline-list-${props.row.name}-view-pipeline`"
                  :icon="outlinedVisibility"
                  class="q-ml-xs"
                  padding="sm"
                  unelevated
                  size="sm"
                  round
                  flat
                  :title="t('pipeline.view')"
                >
                  <q-tooltip position="bottom">
                    <PipelineView :pipeline="props.row" />
                  </q-tooltip>
                </q-btn>
              </template>
            </q-td>
          </q-tr>
          <q-tr
            data-test="scheduled-pipeline-row-expand"
            v-show="expandedRow === props.row.pipeline_id"
            :props="props"
          >
            <q-td v-if="props.row?.sql_query" colspan="100%">
              <div
                data-test="scheduled-pipeline-expanded-content"
                class="text-left tw-px-2 q-mb-sm expanded-content"
              >
                <div class="tw-flex tw-items-center q-py-sm">
                  <strong>SQL Query : <span></span></strong>
                </div>
                <div class="tw-flex tw-items-start tw-justify-center">
                  <div
                    data-test="scheduled-pipeline-expanded-sql"
                    class="scrollable-content expanded-sql"
                  >
                    <pre style="text-wrap: wrap"
                      >{{ props.row?.sql_query }} </pre
                    >
                  </div>
                </div>
              </div>
            </q-td>
          </q-tr>
        </template>
        <template #no-data>
          <no-data />
        </template>
        <template #header-selection="scope">
          <q-checkbox v-model="scope.selected" size="sm" color="secondary" />
        </template>
        <template v-slot:body-selection="scope">
          <q-checkbox v-model="scope.selected" size="sm" color="secondary" />
        </template>

        <template v-slot:body-cell-function="props">
          <q-td :props="props">
            <q-tooltip>
              <pre data-test="scheduled-pipeline-expanded-tooltip-sql">{{
                props.row.sql
              }}</pre>
            </q-tooltip>
            <pre style="white-space: break-spaces">{{ props.row.sql }}</pre>
          </q-td>
        </template>
        <template #top="scope">
          <div class="q-table__title" data-test="pipeline-list-title">
            {{ t("pipeline.header") }}
          </div>
          <div class="tw-flex tw-items-center report-list-tabs q-ml-auto">
            <app-tabs
              data-test="pipeline-list-tabs"
              class="q-mr-md"
              :tabs="tabs"
              v-model:active-tab="activeTab"
              @update:active-tab="updateActiveTab"
            />
            <q-input
              data-test="pipeline-list-search-input"
              v-model="filterQuery"
              borderless
              filled
              dense
              class="q-mb-xs no-border"
              :placeholder="t('pipeline.search')"
            >
              <template #prepend>
                <q-icon name="search" class="cursor-pointer" />
              </template>
            </q-input>
            <q-btn
              data-test="pipeline-list-import-pipeline-btn"
              class="q-ml-md q-mb-xs text-bold"
              padding="sm lg"
              no-caps
              :label="t(`pipeline.import`)"
              @click="routeToImportPipeline"
            />
            <q-btn
              data-test="pipeline-list-add-pipeline-btn"
              class="q-ml-md q-mb-xs text-bold no-border"
              padding="sm lg"
              color="secondary"
              no-caps
              :label="t(`pipeline.addPipeline`)"
              @click="routeToAddPipeline"
            />
          </div>

          <q-table-pagination
            :scope="scope"
            :pageTitle="t('pipeline.header')"
            :position="'top'"
            :resultTotal="resultTotal"
            :perPageOptions="perPageOptions"
            @update:changeRecordPerPage="changePagination"
          />
        </template>

        <template #bottom="scope">
          <div class="bottom-btn">
            <q-btn
              v-if="selectedPipelines.length > 0"
              data-test="pipeline-list-export-pipelines-btn"
              class="flex items-center tw-w-[14vw] no-border"
              color="secondary"
              icon="download"
              :label="'Export'"
              @click="exportBulkPipelines"
            />
            <QTablePagination
              :scope="scope"
              :position="'bottom'"
              :resultTotal="resultTotal"
              :perPageOptions="perPageOptions"
              @update:changeRecordPerPage="changePagination"
            />
          </div>
        </template>
      </q-table>
    </div>
  </div>

  <router-view v-else />

  <q-dialog v-model="showCreatePipeline" position="right" full-height maximized>
    <stream-selection @save="savePipeline" />
  </q-dialog>

  <confirm-dialog
    :title="confirmDialogMeta.title"
    :message="confirmDialogMeta.message"
    @update:ok="confirmDialogMeta.onConfirm()"
    @update:cancel="resetConfirmDialog"
    v-model="confirmDialogMeta.show"
  />
</template>
<script setup lang="ts">
import {
  ref,
  onBeforeMount,
  computed,
  watch,
  reactive,
  onActivated,
  onMounted,
} from "vue";
import { MarkerType } from "@vue-flow/core";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";
import StreamSelection from "./StreamSelection.vue";
import pipelineService from "@/services/pipelines";
import { useStore } from "vuex";
import { useQuasar, type QTableProps } from "quasar";
import type { QTableColumn } from "quasar";

import NoData from "../shared/grid/NoData.vue";
import {
  outlinedDelete,
  outlinedPause,
  outlinedPlayArrow,
  outlinedVisibility,
} from "@quasar/extras/material-icons-outlined";
import QTablePagination from "@/components/shared/grid/Pagination.vue";
import ConfirmDialog from "@/components/ConfirmDialog.vue";
import useDragAndDrop from "@/plugins/pipelines/useDnD";
import AppTabs from "@/components/common/AppTabs.vue";
import PipelineView from "./PipelineView.vue";

import { filter, update } from "lodash-es";

interface Column {
  name: string;
  field: string;
  label: string;
  align: string;
  sortable?: boolean;
}

const { t } = useI18n();
const router = useRouter();

const qTableRef: any = ref({});

const q = useQuasar();

const filterQuery = ref("");

const showCreatePipeline = ref(false);

const expandedRow: any = ref([]); // Array to track expanded rows

const pipelines = ref([]);

const store = useStore();
const isEnabled = ref(false);

const { pipelineObj } = useDragAndDrop();

watch(
  () => router.currentRoute.value,
  async () => {
    await getPipelines();
    updateActiveTab();
  },
);

const confirmDialogMeta: any = ref({
  show: false,
  title: "",
  message: "",
  data: null,
  onConfirm: () => {},
});
const activeTab = ref("all");
const filteredPipelines: any = ref([]);
const columns: any = ref([]);

const tabs = reactive([
  {
    label: "All",
    value: "all",
  },
  {
    label: "Scheduled",
    value: "scheduled",
  },
  {
    label: "Real-Time",
    value: "realtime",
  },
]);

const perPageOptions: any = [
  { label: "5", value: 5 },
  { label: "10", value: 10 },
  { label: "20", value: 20 },
  { label: "50", value: 50 },
  { label: "100", value: 100 },
];
const resultTotal = ref<number>(0);
const maxRecordToReturn = ref<number>(100);
const selectedPerPage = ref<number>(20);
const pagination: any = ref({
  rowsPerPage: 20,
});
const changePagination = (val: { label: string; value: any }) => {
  selectedPerPage.value = val.value;
  pagination.value.rowsPerPage = val.value;
  qTableRef.value?.setPagination(pagination.value);
};

const selectedPipelines = ref<any[]>([]);

const currentRouteName = computed(() => {
  return router.currentRoute.value.name;
});

const filterColumns = (): Column[] => {
  if (activeTab.value === "realtime" || activeTab.value === "all") {
    return columns.value;
  }

  return columns.value.slice(1);
};

const updateActiveTab = () => {
  if (activeTab.value === "all") {
    filteredPipelines.value = pipelines.value.map(
      (pipeline: any, index: any) => ({
        ...pipeline,
        "#": index + 1,
      }),
    );
    columns.value = getColumnsForActiveTab(activeTab.value);
    filteredPipelines.value = pipelines.value;
    resultTotal.value = pipelines.value.length;
    return;
  }

  filteredPipelines.value = pipelines.value
    .filter((pipeline: any) => pipeline.source.source_type === activeTab.value)
    .map((pipeline: any, index) => ({
      ...pipeline,
      "#": index + 1,
    }));

  resultTotal.value = filteredPipelines.value.length;
  columns.value = getColumnsForActiveTab(activeTab.value);
};

const toggleAlertState = (row: any) => {
  const newState = !row.enabled;
  pipelineService
    .toggleState(
      store.state.selectedOrganization.identifier,
      row.pipeline_id,
      newState,
    )
    .then((response) => {
      row.enabled = newState;
      const message = row.enabled
        ? `${row.name} state resumed successfully`
        : `${row.name} state paused successfully`;
      q.notify({
        message: message,
        color: "positive",
        position: "bottom",
        timeout: 3000,
      });
    })
    .catch((error) => {
      if (error.response.status != 403) {
        q.notify({
          message:
            error.response?.data?.message ||
            "Error while updating pipeline state",
          color: "negative",
          position: "bottom",
          timeout: 3000,
        });
      }
    });
};

const triggerExpand = (props: any) => {
  if (
    expandedRow.value === props.row.pipeline_id ||
    props.row.source.source_type === "realtime"
  ) {
    expandedRow.value = null;
  } else {
    // Otherwise, expand the clicked row and collapse any other row
    expandedRow.value = props.row.pipeline_id;
  }
};

const editingPipeline = ref<any | null>(null);

// const updateActiveTab = (tab: string) => {
//   isRealTime.value = tab === "realTime";
// };

const getColumnsForActiveTab = (tab: any) => {
  let realTimeColumns = [
    { name: "#", label: "#", field: "#", align: "left" },

    {
      name: "name",
      field: "name",
      label: t("common.name"),
      align: "left",
      sortable: true,
    },
    {
      name: "stream_name",
      field: "stream_name",
      label: t("alerts.stream_name"),
      align: "left",
      sortable: true,
    },
    {
      name: "stream_type",
      field: "stream_type",
      label: t("alerts.streamType"),
      align: "left",
      sortable: true,
    },
  ];

  let scheduledColumns = [
    { name: "#", label: "#", field: "#", align: "left" },

    {
      name: "name",
      field: "name",
      label: t("common.name"),
      align: "left",
      sortable: true,
    },
    {
      name: "stream_type",
      field: "stream_type",
      label: "Stream Type",
      align: "left",
      sortable: true,
    },
    {
      name: "frequency",
      field: "frequency",
      label: "Frequency",
      align: "left",
      sortable: true,
    },
    {
      name: "period",
      field: "period",
      label: "Period",
      align: "left",
      sortable: true,
    },
    {
      name: "cron",
      field: "cron",
      label: "Cron",
      align: "left",
      sortable: false,
    },
    {
      name: "sql_query",
      field: "sql_query",
      label: "SQL Query",
      align: "left",
      sortable: false,
      style:
        "white-space: no-wrap; max-width: 200px; overflow: hidden; text-overflow: ellipsis;",
    },
  ];

  const actionsColumn = {
    name: "actions",
    field: "actions",
    label: t("alerts.actions"),
    align: "center",
    sortable: false,
  };
  if (tab === "all") {
    const allColumns = [...scheduledColumns, actionsColumn];
    allColumns.splice(2, 0, {
      name: "type",
      field: "type",
      label: "Type",
      align: "left",
      sortable: true,
    });

    allColumns.splice(3, 0, {
      name: "stream_name",
      field: "stream_name",
      label: t("alerts.stream_name"),
      align: "left",
      sortable: true,
    });

    return allColumns;
  }
  return tab === "realtime"
    ? [...realTimeColumns, actionsColumn]
    : [...scheduledColumns, actionsColumn];
};

onMounted(async () => {
  await getPipelines(); // Ensure pipelines are fetched before updating
  updateActiveTab();
});

const createPipeline = () => {
  showCreatePipeline.value = true;
};

const getPipelines = async () => {
  try {
    const response = await pipelineService.getPipelines(
      store.state.selectedOrganization.identifier,
    );
    pipelines.value = [];
    // resultTotal.value = response.data.list.length;
    pipelines.value = response.data.list.map((pipeline: any, index: any) => {
      const updatedEdges = pipeline.edges.map((edge: any) => ({
        ...edge,
        markerEnd: {
          type: MarkerType.ArrowClosed,
          width: 20, // Increase arrow width
          height: 20, // Increase arrow height
        },
        type: "custom",

        style: {
          strokeWidth: 2,
        },
        animated: true,
        updatable: true,
      }));
      pipeline.type = pipeline.source.source_type;
      if (pipeline.source.source_type === "realtime") {
        pipeline.stream_name = pipeline.source.stream_name;
        pipeline.stream_type = pipeline.source.stream_type;
      } else {
        pipeline.stream_type = pipeline.source.stream_type;
        pipeline.frequency =
          pipeline.source.trigger_condition.frequency_type == "minutes"
            ? pipeline.source.trigger_condition.frequency + " Mins"
            : pipeline.source.trigger_condition.cron;
        pipeline.period = pipeline.source.trigger_condition.period + " Mins";
        pipeline.cron =
          pipeline.source.trigger_condition.frequency_type == "minutes"
            ? "False"
            : "True";
        pipeline.sql_query = pipeline.source.query_condition.sql;
      }

      pipeline.edges = updatedEdges;
      return {
        ...pipeline,
        "#": index + 1,
      };
    });
  } catch (error) {
    console.error(error);
  }
};
const editPipeline = (pipeline: any) => {
  pipeline.nodes.forEach((node: any) => {
    node.type = node.io_type;
  });

  pipelineObj.currentSelectedPipeline = pipeline;
  pipelineObj.pipelineWithoutChange = pipeline;
  router.push({
    name: "pipelineEditor",
    query: {
      id: pipeline.pipeline_id,
      name: pipeline.name,
      org_identifier: store.state.selectedOrganization.identifier,
    },
  });
};

const openDeleteDialog = (pipeline: any) => {
  confirmDialogMeta.value.show = true;
  confirmDialogMeta.value.title = t("pipeline.deletePipeline");
  confirmDialogMeta.value.message =
    "Are you sure you want to delete this pipeline?";
  confirmDialogMeta.value.onConfirm = deletePipeline;
  confirmDialogMeta.value.data = pipeline;
};

const savePipeline = (data: any) => {
  const dismiss = q.notify({
    message: "saving pipeline...",
    position: "bottom",
    spinner: true,
  });

  pipelineService
    .createPipeline({
      ...data,
      org_identifier: store.state.selectedOrganization.identifier,
    })
    .then(() => {
      getPipelines();
      dismiss();
      showCreatePipeline.value = false;
      q.notify({
        message: "Pipeline created successfully",
        color: "positive",
        position: "bottom",
        timeout: 3000,
      });
    })
    .catch((error) => {
      dismiss();
      if (error.response.status != 403) {
        q.notify({
          message:
            error.response?.data?.message || "Error while saving pipeline",
          color: "negative",
          position: "bottom",
          timeout: 3000,
        });
      }
    });
};

const deletePipeline = async () => {
  const dismiss = q.notify({
    message: "deleting pipeline...",
    position: "bottom",
    spinner: true,
  });
  const { pipeline_id } = confirmDialogMeta.value.data;
  const org_id = store.state.selectedOrganization.identifier;
  pipelineService
    .deletePipeline({
      pipeline_id,
      org_id,
    })
    .then(async () => {
      q.notify({
        message: "Pipeline deleted successfully",
        color: "positive",
        position: "bottom",
        timeout: 3000,
      });
    })
    .catch((error) => {
      if (error.response.status != 403) {
        q.notify({
          message:
            error.response?.data?.message || "Error while deleting pipeline",
          color: "negative",
          position: "bottom",
          timeout: 3000,
        });
      }
    })
    .finally(async () => {
      selectedPipelines.value = [];
      await getPipelines();
      updateActiveTab();
      dismiss();
    });

  resetConfirmDialog();
};

const resetConfirmDialog = () => {
  confirmDialogMeta.value.show = false;
  confirmDialogMeta.value.title = "";
  confirmDialogMeta.value.message = "";
  confirmDialogMeta.value.onConfirm = () => {};
  confirmDialogMeta.value.data = null;
};

const filterData = (rows: any, terms: any) => {
  var filtered = [];
  terms = terms.toLowerCase();
  for (var i = 0; i < rows.length; i++) {
    if (rows[i]["name"].toLowerCase().includes(terms)) {
      filtered.push(rows[i]);
    }
  }
  return filtered;
};
const routeToAddPipeline = () => {
  router.push({
    name: "createPipeline",
    query: {
      org_identifier: store.state.selectedOrganization.identifier,
    },
  });
};

const exportPipeline = (row: any) => {
  const pipelineToBeExported = row.name;

  const pipelineJson = JSON.stringify(row, null, 2);
  // Create a Blob from the JSON string
  const blob = new Blob([pipelineJson], { type: "application/json" });

  // Create an object URL for the Blob
  const url = URL.createObjectURL(blob);

  // Create an anchor element to trigger the download
  const link = document.createElement("a");
  link.href = url;

  // Set the filename of the download
  link.download = `${pipelineToBeExported}.json`;

  // Trigger the download by simulating a click
  link.click();

  // Clean up the URL object after download
  URL.revokeObjectURL(url);
};

const routeToImportPipeline = () => {
  router.push({
    name: "importPipeline",
    query: {
      org_identifier: store.state.selectedOrganization.identifier,
    },
  });
};

const exportBulkPipelines = () => {
  // Create an array of selected pipelines without modifying their structure
  const pipelinesToExport = selectedPipelines.value;

  const exportJson = JSON.stringify(pipelinesToExport, null, 2);
  const blob = new Blob([exportJson], { type: "application/json" });

  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  const date = new Date().toISOString().split("T")[0];
  link.download = `pipelines_export_${date}.json`;

  link.click();

  URL.revokeObjectURL(url);

  selectedPipelines.value = [];
  q.notify({
    message: `${pipelinesToExport.length} pipelines exported successfully`,
    color: "positive",
    position: "bottom",
    timeout: 3000,
  });
};
</script>
<style lang="scss" scoped>
.pipeline-list-table {
  th:last-child,
  td:last-child {
    position: sticky;
    right: 0;
    z-index: 1;
    box-shadow: -4px 0px 4px 0 rgba(0, 0, 0, 0.1);
    width: 100px;
  }
}

.dark-theme {
  th:last-child,
  td:last-child {
    background: var(--q-dark);
    box-shadow: -4px 0px 4px 0 rgba(144, 144, 144, 0.1);
    width: 100px;
  }
}

.light-theme {
  th:last-child,
  td:last-child {
    background: #ffffff;
    width: 100px;
  }
}
.dark-mode {
  background-color: $dark-page;

  .report-list-tabs {
    height: fit-content;

    :deep(.rum-tabs) {
      border: 1px solid #464646;
    }

    :deep(.rum-tab) {
      &:hover {
        background: #464646;
      }

      &.active {
        background: #5960b2;
        color: #ffffff !important;
      }
    }
  }
}
.report-list-tabs {
  height: fit-content;

  :deep(.rum-tabs) {
    border: 1px solid #eaeaea;
    height: fit-content;
    border-radius: 4px;
    overflow: hidden;
  }

  :deep(.rum-tab) {
    width: fit-content !important;
    padding: 4px 12px !important;
    border: none !important;

    &:hover {
      background: #eaeaea;
    }

    &.active {
      background: #5960b2;
      color: #ffffff !important;
    }
  }
}

.expanded-content {
  padding: 0 3rem;
  max-height: 100vh; /* Set a fixed height for the container */
  overflow: hidden; /* Hide overflow by default */
}

.scrollable-content {
  width: 100%; /* Use the full width of the parent */
  overflow-y: auto; /* Enable vertical scrolling for long content */
  padding: 10px; /* Optional: padding for aesthetics */
  border: 1px solid #ddd; /* Optional: border for visibility */
  height: 100%;
  max-height: 200px;
  /* Use the full height of the parent */
  text-wrap: normal;
  background-color: #e8e8e8;
  color: black;
}
.expanded-sql {
  border-left: #7a54a2 3px solid;
}

:deep(.pipeline-list-table thead th:last-child) {
  position: sticky;
  right: 0;
  z-index: 1;
  box-shadow: -4px 0px 4px 0 rgba(0, 0, 0, 0.1);
  width: 100px;
}

:deep(.dark-theme.pipeline-list-table thead th:last-child) {
  background: var(--q-dark);
}
:deep(.light-theme.pipeline-list-table thead th:last-child) {
  background: #ffffff;
}
.bottom-btn {
  display: flex;
  width: 100%;
  justify-content: space-between;
  align-items: center;
}
</style>
