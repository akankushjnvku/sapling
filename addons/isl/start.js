/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

const {injectAdditionalPlatforms} = require('./customBuildEntry');
const rewire = require('rewire');
const defaults = rewire('react-scripts/scripts/start.js');
const configFactory = defaults.__get__('configFactory');

defaults.__set__('configFactory', env => {
  const config = configFactory(env);
  config.experiments = {
    asyncWebAssembly: true,
  };
  config.output.library = 'EdenSmartlog';

  // don't open broser when running `yarn start`,
  // since we need to use `yarn serve --dev` from isl-server
  process.env.BROWSER = 'none';

  injectAdditionalPlatforms(config);

  // ts-loader is required to reference external typescript projects/files (non-transpiled)
  config.module.rules.push({
    test: /\.tsx?$/,
    loader: 'ts-loader',
    exclude: /node_modules/,
    options: {
      transpileOnly: true,
      configFile: 'tsconfig.json',
    },
  });

  return config;
});