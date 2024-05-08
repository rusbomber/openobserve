// Copyright 2023 Zinc Labs Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

import config from "@/aws-exports";
const useLazyLoad = () => {
  const loadNodeSQLParser = async () => {
    const module: any = await import("node-sql-parser/build/mysql");
    const Parser: any = module.Parser;
    return Parser;
  };

  // const loadVue3GridLayout = async () => {
  //   const VueGridLayout: any = await import("vue3-grid-layout");
  //   return {
  //     GridLayout: VueGridLayout.GridLayout,
  //     GridItem: VueGridLayout.GridItem,
  //     default: VueGridLayout.default,
  //   };
  // };

  // const loadRudderSDK = async () => {
  //   const writeKey = "ziox-cloud-browser";
  //   const dataPlaneUrl = "https://e1.zinclabs.dev";
  //   const rudderanalytics: any = await import("rudder-sdk-js");
  //   if (config.enableAnalytics == "true") {
  //     rudderanalytics.ready(() => {
  //       console.log("we are all set!!!");
  //     });

  //     rudderanalytics.load(writeKey, dataPlaneUrl, {
  //       configUrl: "https://e1.zinclabs.dev/v1/config",
  //     });
  //   }
  //   return rudderanalytics;
  // };

  // const loadMonaco = async () => {
  //   const module = await import("monaco-editor");
  //   console.log(module);
  //   return module;
  // };

  return {
    loadNodeSQLParser,
  };
};

export default useLazyLoad;
