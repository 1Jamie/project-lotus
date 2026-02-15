#!/usr/bin/env node

const { Command } = require('commander');
const { spawn } = require('child_process');
const chokidar = require('chokidar');
const path = require('path');
const fs = require('fs');

const program = new Command();

program
    .name('lotus')
    .description('Lotus Project CLI')
    .version('0.1.0');

program
    .command('dev [entry]')
    .description('Launch the Lotus runner with hot-reloading')
    .action((entry = 'index.js') => {
        console.log(`Starting Lotus dev server...`);

        let appProcess = null;

        const startApp = () => {
            if (appProcess) {
                appProcess.kill();
            }

            console.log(`Launching ${entry}...`);
            appProcess = spawn('node', [entry], {
                stdio: 'inherit',
                env: { ...process.env, LOTUS_DEV: 'true' }
            });

            appProcess.on('close', (code) => {
                if (code !== 0 && code !== null) {
                    console.log(`Lotus app exited with code ${code}`);
                }
            });
        };

        const watcher = chokidar.watch('.', {
            ignored: /(^|[\/\\])\..|node_modules|dist/,
            persistent: true
        });

        watcher.on('change', (path) => {
            console.log(`File ${path} has been changed`);
            startApp();
        });

        startApp();
    });

program
    .command('clean')
    .description('Remove build artifacts (dist/ directory)')
    .action(() => {
        const distDir = path.resolve('dist');
        if (fs.existsSync(distDir)) {
            fs.rmSync(distDir, { recursive: true, force: true });
            console.log('Cleaned dist/ directory.');
        } else {
            console.log('Nothing to clean.');
        }
    });

program
    .command('build')
    .description('Build the application for production')
    .option('--platform <platform>', 'Target platform (linux, win32)', process.platform)
    .option('--target <target>', 'Target format (deb, rpm)', 'deb')
    .action(async (cmdOptions) => {
        const platform = cmdOptions.platform;
        const target = cmdOptions.target;
        console.log(`Building for ${platform} (${target})...`);

        const configPath = path.resolve('lotus.config.json');
        if (!fs.existsSync(configPath)) {
            console.error('Error: lotus.config.json not found.');
            process.exit(1);
        }

        const config = JSON.parse(fs.readFileSync(configPath, 'utf8'));
        const distDir = path.resolve('dist');
        const appDir = path.join(distDir, 'app');
        const resourcesDir = path.join(appDir, 'resources', 'app');

        // Clean dist
        if (fs.existsSync(distDir)) {
            fs.rmSync(distDir, { recursive: true, force: true });
        }
        fs.mkdirSync(resourcesDir, { recursive: true });

        console.log('Copying application files...');

        // For copying app's own files — skip build artifacts, dev dirs
        const copyAppFiles = (src, dest) => {
            if (fs.statSync(src).isDirectory()) {
                if (!fs.existsSync(dest)) fs.mkdirSync(dest);
                fs.readdirSync(src).forEach(child => {
                    if (child === 'dist' || child === '.git' || child === '.github' || child === 'node_modules' || child === 'packages' || child === '.DS_Store' || child === 'target' || child === 'servo') return;
                    copyAppFiles(path.join(src, child), path.join(dest, child));
                });
            } else {
                fs.copyFileSync(src, dest);
            }
        };
        copyAppFiles(process.cwd(), resourcesDir);

        // For copying node_modules — don't skip dist, only skip .git
        const copyModuleFiles = (src, dest) => {
            if (fs.statSync(src).isDirectory()) {
                if (!fs.existsSync(dest)) fs.mkdirSync(dest);
                fs.readdirSync(src).forEach(child => {
                    if (child === '.git') return;
                    copyModuleFiles(path.join(src, child), path.join(dest, child));
                });
            } else {
                fs.copyFileSync(src, dest);
            }
        };

        const copyNodeModules = (src, dest) => {
            if (!fs.existsSync(src)) return;
            if (!fs.existsSync(dest)) fs.mkdirSync(dest);
            fs.readdirSync(src).forEach(child => {
                if (child.startsWith('.')) return;
                copyModuleFiles(path.join(src, child), path.join(dest, child));
            });
        };
        copyNodeModules(path.join(process.cwd(), 'node_modules'), path.join(resourcesDir, 'node_modules'));

        // Create version file for electron-installer-debian in the ROOT of the appDir (it expects it there)
        fs.writeFileSync(path.join(appDir, 'version'), config.version || '0.1.0');

        // Handle LICENSE file (required by electron-installer-debian)
        const licenseSrc = ['LICENSE', 'LICENSE.md', 'LICENSE.txt'].find(f => fs.existsSync(f));
        if (licenseSrc) {
            fs.copyFileSync(licenseSrc, path.join(appDir, 'LICENSE'));
        } else {
            // Create a placeholder license if none exists
            fs.writeFileSync(path.join(appDir, 'LICENSE'), `Copyright (c) ${new Date().getFullYear()} ${config.author || 'Lotus App Developer'}. All rights reserved.`);
        }

        // Verify @lotus-gui/core binary
        // ...

        if (platform === 'linux') {
            // Determine binary name
            const binName = config.executableName || config.name.toLowerCase().replace(/ /g, '-');
            const wmClass = config.build?.linux?.wmClass || binName;

            // Use platform-appropriate arch and dependency names
            const isRpm = target === 'rpm';
            const arch = isRpm ? 'x86_64' : 'amd64';
            const deps = isRpm
                ? ['nodejs', 'openssl-libs', 'gtk3', 'webkit2gtk4.0']
                : ['nodejs', 'libssl-dev', 'libgtk-3-0', 'libwebkit2gtk-4.0-37'];

            const options = {
                src: appDir,
                dest: path.join(distDir, 'installers'),
                arch: arch,
                name: binName,
                productName: config.name,
                genericName: config.name,
                version: config.version,
                description: config.description,
                productDescription: config.description || config.name,
                icon: config.icon ? path.resolve(config.icon) : undefined,
                section: config.build?.linux?.section || 'utils',
                categories: config.build?.linux?.categories || ['Utility'],
                bin: binName,
                depends: deps,
                maintainer: config.author || 'Lotus App Developer',
                homepage: config.homepage,
                priority: 'optional',
                license: config.license || 'Proprietary'
            };

            // Determine entry point: lotus.config.json > package.json > index.js
            const appPackageCtx = JSON.parse(fs.readFileSync(path.join(resourcesDir, 'package.json'), 'utf8'));
            const entryPoint = config.main || appPackageCtx.main || 'index.js';

            // Create Wrapper Script at the ROOT of appDir (which will be installed to /usr/lib/APPNAME/)
            // The script needs to execute node on the file in resources/app
            const binScriptPath = path.join(appDir, binName);

            // NOTE: electron-installer-debian installs 'src' content into '/usr/lib/<options.name>/'
            // So our resources are at '/usr/lib/<options.name>/resources/app'
            // And our binary (this script) is at '/usr/lib/<options.name>/<binName>'

            const wrapperScript = `#!/bin/sh
exec node "/usr/lib/${options.name}/resources/app/${entryPoint}" "$@"
`;
            fs.writeFileSync(binScriptPath, wrapperScript, { mode: 0o755 });

            try {
                if (target === 'rpm') {
                    console.log('Creating RPM package...');
                    const { Installer } = require('electron-installer-redhat');
                    const common = require('electron-installer-common');

                    class RPMInstaller extends Installer {
                        async createSpec() {
                            // Point to our custom template in packages/lotus-dev/lib/templates/spec.ejs
                            const templatePath = path.resolve(__dirname, '../lib/templates/spec.ejs');
                            this.options.logger(`Creating spec file at ${this.specPath} using custom template`);
                            return common.wrapError('creating spec file', async () => this.createTemplatedFile(templatePath, this.specPath));
                        }
                    }

                    // Replicate module.exports logic from electron-installer-redhat
                    const buildRpm = async (data) => {
                        // Mock logger
                        data.logger = data.logger || ((msg) => console.log(msg));

                        // Mock rename function (default from electron-installer-redhat)
                        data.rename = data.rename || function (dest, src) {
                            return path.join(dest, '<%= name %>-<%= version %>-<%= revision %>.<%= arch %>.rpm');
                        };

                        const installer = new RPMInstaller(data);
                        await installer.generateDefaults();
                        await installer.generateOptions();
                        await installer.generateScripts();
                        await installer.createStagingDir();
                        await installer.createContents();
                        await installer.createPackage();
                        await installer.movePackage();
                        return installer.options;
                    };

                    // RPM specific adjustments
                    options.requires = options.depends; // RPM uses 'requires', not 'depends'
                    delete options.depends;

                    await buildRpm(options);
                } else { // Default to debian
                    console.log('Creating Debian package...');
                    const installer = require('electron-installer-debian');
                    await installer(options);
                }
                console.log(`Successfully created package at ${options.dest}`);
            } catch (err) {
                console.error(err, err.stack);
                process.exit(1);
            }
        } else {
            console.log('Packager for this platform not fully implemented yet.');
        }

    });

program.parse();
