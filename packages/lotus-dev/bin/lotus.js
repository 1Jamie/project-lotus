#!/usr/bin/env node

const { Command } = require('commander');
const { spawn, execSync } = require('child_process');
const chokidar = require('chokidar');
const path = require('path');
const fs = require('fs');
const prompts = require('prompts');
const os = require('os');
const { Jimp } = require('jimp');

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
    .option('--target <target>', 'Target format (deb, appimage, msi, nsis)', 'deb')
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

        // Clean dist
        if (fs.existsSync(distDir)) {
            fs.rmSync(distDir, { recursive: true, force: true });
        }
        fs.mkdirSync(appDir, { recursive: true });

        // Determine entry point: lotus.config.json > package.json > index.js
        const appPackagePath = path.resolve('package.json');
        let entryPoint = 'index.js';
        if (fs.existsSync(appPackagePath)) {
            const appPackageCtx = JSON.parse(fs.readFileSync(appPackagePath, 'utf8'));
            entryPoint = config.main || appPackageCtx.main || 'index.js';
        }

        // 1. Bundle JS with esbuild
        console.log('Bundling application with esbuild...');
        const bundlePath = path.join(appDir, 'bundle.js');
        const shimPath = path.join(appDir, 'esbuild-shim.js');

        try {
            // Write a shim to intercept __dirname logic for native modules
            fs.writeFileSync(shimPath, `
import fs from 'fs';
import path from 'path';
const execDir = path.dirname(process.execPath);
const libDir = path.join('/usr/lib', path.basename(process.execPath));
const flatpakLibDir = path.join('/app/lib', path.basename(process.execPath));
let macroDir = execDir;
if (fs.existsSync(path.join(libDir, 'msgpackr-renderer.js'))) {
    macroDir = libDir;
} else if (fs.existsSync(path.join(flatpakLibDir, 'msgpackr-renderer.js'))) {
    macroDir = flatpakLibDir;
}
export const __dirname_macro = macroDir;
            `.trim());

            // We use esbuild to bundle, and specifically override __dirname so native modules
            // can resolve their .node files sitting next to the final executable.
            execSync(`npx esbuild "${entryPoint}" --bundle --platform=node --outfile="${bundlePath}" --external:*.node --inject:"${shimPath}" --define:__dirname=__dirname_macro`, { stdio: 'inherit' });

            fs.unlinkSync(shimPath); // Clean up shim

            // Node SEA fails to resolve built-in `require("./file.node")` relative to the VFS. 
            // It also forbids requiring external modules via the global `require` wrapper.
            // We patch the bundle to force an absolute module path relative to our execPath,
            // loaded using `module.createRequire` which escapes the SEA sandbox constraint.
            // When installed via .deb or .rpm, resources are in /usr/lib/<appName>/ instead of /usr/bin/
            let bundleContent = fs.readFileSync(bundlePath, 'utf8');
            bundleContent = bundleContent.replace(/require\(['"]\.\/([^'"]+\.node)['"]\)/g, "require('module').createRequire(process.execPath)(require('path').join(__dirname_macro, '$1'))");
            fs.writeFileSync(bundlePath, bundleContent);
        } catch (err) {
            console.error('esbuild failed');
            process.exit(1);
        }

        // 2. Crawl and copy .node files
        console.log('Extracting native .node modules...');
        const findNodeFiles = (dir, fileList = [], visited = new Set()) => {
            if (!fs.existsSync(dir)) return fileList;
            const realDir = fs.realpathSync(dir);
            if (visited.has(realDir)) return fileList;
            visited.add(realDir);

            let files = [];
            try {
                files = fs.readdirSync(dir);
            } catch (err) {
                if (err.code === 'EACCES' || err.code === 'EPERM') return fileList;
                throw err;
            }

            for (const file of files) {
                const fullPath = path.join(dir, file);
                if (file === '.git' || file === '.github' || file === '.flatpak-builder') continue;

                try {
                    if (fs.statSync(fullPath).isDirectory()) {
                        findNodeFiles(fullPath, fileList, visited);
                    } else if (file.endsWith('.node')) {
                        fileList.push(fullPath);
                    }
                } catch (err) {
                    if (err.code !== 'EACCES' && err.code !== 'EPERM') throw err;
                }
            }
            return fileList;
        };

        const nodeFiles = findNodeFiles(path.resolve('node_modules'));
        for (const file of nodeFiles) {
            const fileName = path.basename(file);
            fs.copyFileSync(file, path.join(appDir, fileName));
            console.log(`Copied ${fileName}`);
        }

        console.log('Extracting msgpackr renderer script...');
        let msgpackrRendererPath;
        try {
            msgpackrRendererPath = require.resolve('msgpackr/dist/index.min.js', { paths: [process.cwd()] });
        } catch (e) {
            try {
                msgpackrRendererPath = path.join(path.dirname(require.resolve('msgpackr', { paths: [process.cwd()] })), 'index.min.js');
            } catch (e2) {
                console.warn('Could not locate msgpackr in node_modules! IPC may fail.');
            }
        }
        if (msgpackrRendererPath && fs.existsSync(msgpackrRendererPath)) {
            fs.copyFileSync(msgpackrRendererPath, path.join(appDir, 'msgpackr-renderer.js'));
            console.log('Copied msgpackr-renderer.js');
        }

        // 3. Node SEA Generation
        console.log('Generating Node SEA...');
        const binName = config.executableName || config.name.toLowerCase().replace(/ /g, '-');
        const isWindows = platform === 'win32';
        const binPath = isWindows ? path.join(appDir, `${binName}.exe`) : path.join(appDir, binName);

        const seaConfigPath = path.join(appDir, 'sea-config.json');
        const seaConfig = {
            main: bundlePath,
            output: path.join(appDir, 'sea-prep.blob'),
            disableExperimentalSEAWarning: true
        };
        fs.writeFileSync(seaConfigPath, JSON.stringify(seaConfig, null, 2));

        try {
            execSync(`node --experimental-sea-config "${seaConfigPath}"`, { stdio: 'inherit' });

            // Copy Node binary
            fs.copyFileSync(process.execPath, binPath);
            if (!isWindows) fs.chmodSync(binPath, 0o755);

            // Inject blob
            // The sentinel fuse is hardcoded per Node.js documentation for SEA
            execSync(`npx postject "${binPath}" NODE_SEA_BLOB "${seaConfig.output}" --sentinel-fuse NODE_SEA_FUSE_fce680ab2cc467b6e072b8b5df1996b2`, { env: { ...process.env }, stdio: 'inherit' });

            // Cleanup temp SEA files
            fs.unlinkSync(bundlePath);
            fs.unlinkSync(seaConfig.output);
            fs.unlinkSync(seaConfigPath);
        } catch (err) {
            console.error('SEA generation failed', err);
            process.exit(1);
        }

        // 4. CrabNebula Packaging
        console.log('Packaging with CrabNebula...');
        const packagerConfigPath = path.join(distDir, 'packager.json');

        const packagerConfig = {
            productName: config.name,
            version: config.version,
            description: config.description || config.name,
            identifier: `com.lotus.${binName}`,
            authors: [config.author || 'Lotus Dev'],
            outDir: path.resolve(distDir, 'installers'),
            // CrabNebula requires paths relative to the current directory where it runs
            binariesDir: path.relative(distDir, appDir),
            binaries: [
                {
                    path: isWindows ? binName + '.exe' : binName,
                    main: true
                }
            ],
            // Instruct packager to copy our native `.node` modules together into the final installation directory
            resources: [
                path.join(path.relative(distDir, appDir), '*.node'),
                path.join(path.relative(distDir, appDir), 'msgpackr-renderer.js')
            ],
            deb: {
                depends: []
            }
        };

        if (config.resources && Array.isArray(config.resources)) {
            for (const res of config.resources) {
                const resPath = path.resolve(process.cwd(), res);
                if (fs.existsSync(resPath)) {
                    packagerConfig.resources.push(resPath);
                }
            }
        }

        if (config.icon) {
            packagerConfig.icons = [path.resolve(config.icon)];
        }

        fs.writeFileSync(packagerConfigPath, JSON.stringify(packagerConfig, null, 2));
        const setupAppDir = async (targetBuildSystem = target) => {
            const appDirName = path.join(distDir, 'AppDir');
            if (fs.existsSync(appDirName)) return appDirName;

            const appId = config.appId || `org.lotus.${binName}`;
            const desktopIconName = targetBuildSystem === 'flatpak' ? appId : binName;

            const libDir = path.join(appDirName, 'usr', 'lib', binName);
            fs.mkdirSync(path.join(appDirName, 'usr', 'bin'), { recursive: true });
            fs.mkdirSync(libDir, { recursive: true });

            // Copy the binary into usr/bin/
            fs.copyFileSync(binPath, path.join(appDirName, 'usr', 'bin', binName));
            if (!isWindows) fs.chmodSync(path.join(appDirName, 'usr', 'bin', binName), 0o755);

            // Copy .node files and msgpackr into usr/lib/<binName>/ (FHS-compliant, matches __dirname_macro)
            for (const file of nodeFiles) {
                fs.copyFileSync(file, path.join(libDir, path.basename(file)));
            }
            const localMsgpackr = path.join(appDir, 'msgpackr-renderer.js');
            if (fs.existsSync(localMsgpackr)) {
                fs.copyFileSync(localMsgpackr, path.join(libDir, 'msgpackr-renderer.js'));
            }

            // Copy extra resources into usr/lib/<binName>/
            if (config.resources && Array.isArray(config.resources)) {
                for (const res of config.resources) {
                    const resPath = path.resolve(process.cwd(), res);
                    if (fs.existsSync(resPath)) {
                        const destPath = path.join(libDir, path.basename(res));
                        if (fs.statSync(resPath).isDirectory()) {
                            fs.cpSync(resPath, destPath, { recursive: true });
                        } else {
                            fs.copyFileSync(resPath, destPath);
                        }
                    }
                }
            }

            // Create AppRun (for AppImage — launches via the wrapper)
            const appRunPath = path.join(appDirName, 'AppRun');
            fs.writeFileSync(appRunPath, `#!/bin/sh\nHERE="$(dirname "$(readlink -f "\${0}")")"\nexport LD_LIBRARY_PATH="\${HERE}/usr/lib:\${LD_LIBRARY_PATH}"\nexec "\${HERE}/usr/bin/${binName}" "$@"\n`);
            fs.chmodSync(appRunPath, 0o755);

            // Create .desktop
            const desktopContent = `[Desktop Entry]\nName=${config.name}\nExec=${binName}\nIcon=${desktopIconName}\nType=Application\nCategories=Utility;\n`;
            fs.writeFileSync(path.join(appDirName, `${binName}.desktop`), desktopContent);

            // Create icon
            if (config.icon && fs.existsSync(path.resolve(config.icon))) {
                const iconPath = path.resolve(config.icon);
                const ext = path.extname(iconPath) || '.png';
                fs.copyFileSync(iconPath, path.join(appDirName, `${binName}${ext}`));
                fs.copyFileSync(iconPath, path.join(appDirName, `.DirIcon`));
            } else {
                fs.writeFileSync(path.join(appDirName, `${binName}.png`), 'iVBO... (empty icon placeholder)');
                fs.writeFileSync(path.join(appDirName, `.DirIcon`), 'empty');
            }

            // Expose standard FreeDesktop paths for RPM and Flatpak
            const appsDir = path.join(appDirName, 'usr', 'share', 'applications');
            const iconsDir = path.join(appDirName, 'usr', 'share', 'icons', 'hicolor', '512x512', 'apps');
            fs.mkdirSync(appsDir, { recursive: true });
            fs.mkdirSync(iconsDir, { recursive: true });
            fs.writeFileSync(path.join(appsDir, `${binName}.desktop`), desktopContent);
            if (config.icon && fs.existsSync(path.resolve(config.icon))) {
                const ext = path.extname(path.resolve(config.icon)) || '.png';
                try {
                    const image = await Jimp.read(path.resolve(config.icon));
                    await image.resize({ w: 512, h: 512 });
                    await image.write(path.join(iconsDir, `${binName}${ext}`));
                } catch (e) {
                    console.error("Failed to resize icon for Flatpak. Skipping...", e);
                    fs.copyFileSync(path.resolve(config.icon), path.join(iconsDir, `${binName}${ext}`));
                }
            }

            return appDirName;
        };

        try {
            if (target === 'appimage') {
                console.log('Building AppImage natively...');

                const toolsDir = path.join(os.homedir(), '.lotus-gui', 'tools');
                fs.mkdirSync(toolsDir, { recursive: true });
                const appImageToolPath = path.join(toolsDir, 'appimagetool');

                if (!fs.existsSync(appImageToolPath)) {
                    console.log('Downloading appimagetool... (this only happens once)');
                    try {
                        execSync(`wget -qO "${appImageToolPath}" https://github.com/AppImage/AppImageKit/releases/download/continuous/appimagetool-x86_64.AppImage`, { stdio: 'inherit' });
                        fs.chmodSync(appImageToolPath, 0o755);
                    } catch (e) {
                        try {
                            execSync(`curl -sL -o "${appImageToolPath}" https://github.com/AppImage/AppImageKit/releases/download/continuous/appimagetool-x86_64.AppImage`, { stdio: 'inherit' });
                            fs.chmodSync(appImageToolPath, 0o755);
                        } catch (e2) {
                            console.error('Failed to download appimagetool. Please install curl or wget.');
                            process.exit(1);
                        }
                    }
                }

                const appDirName = await setupAppDir();

                // Run appimagetool
                console.log('Running appimagetool...');
                const installersDir = path.join(distDir, 'installers');
                fs.mkdirSync(installersDir, { recursive: true });
                const outPath = path.join(installersDir, `${binName}-${config.version}-x86_64.AppImage`);
                execSync(`"${appImageToolPath}" "${appDirName}" "${outPath}"`, { stdio: 'inherit', env: { ...process.env, ARCH: 'x86_64' } });
                console.log(`Successfully created packages in ${installersDir}`);

            } else {
                // Provide format argument to crabnebula packager
                // CrabNebula formats: deb, appimage, pacman (Linux); wix, nsis (Windows); app, dmg (Mac).
                // Default to target if specified, else crabnebula chooses.
                let formatArg = '';
                if (target === 'deb' || target === 'appimage' || target === 'nsis' || target === 'wix' || target === 'msi' || target === 'exe' || target === 'pacman') {
                    const crabTarget = target === 'msi' ? 'wix' : target === 'exe' ? 'nsis' : target;
                    formatArg = `-f ${crabTarget}`;
                } else if (target === 'rpm') {
                    console.log('Building manual RPM via rpmbuild...');
                    const installersDir = path.join(distDir, 'installers');
                    fs.mkdirSync(installersDir, { recursive: true });

                    const appDirName = await setupAppDir();

                    const rpmBuildDir = path.join(distDir, 'rpmbuild');
                    const rpmBuildRoot = path.join(rpmBuildDir, 'BUILDROOT');
                    fs.mkdirSync(path.join(rpmBuildDir, 'SPECS'), { recursive: true });
                    fs.mkdirSync(path.join(rpmBuildDir, 'RPMS'), { recursive: true });

                    let filesList = '/usr/bin/*\n';
                    filesList += `/usr/lib/${binName}/*\n`;  // .node files live here
                    if (fs.existsSync(path.join(appDirName, 'usr', 'share', 'applications'))) {
                        filesList += '/usr/share/applications/*\n';
                    }
                    if (fs.existsSync(path.join(appDirName, 'usr', 'share', 'icons'))) {
                        filesList += '/usr/share/icons/*\n';
                    }

                    const specContent = `
%define _unpackaged_files_terminate_build 0
%define __os_install_post %{nil}
%define debug_package %{nil}
%global __strip /bin/true
%global _build_id_links none

Name:           ${config.name}
Version:        ${config.version}
Release:        1%{?dist}
Summary:        ${config.description || config.name}
License:        ${config.license || 'Proprietary'}
${config.homepage ? `URL:            ${config.homepage}` : ''}
BuildArch:      x86_64
AutoReqProv:    no

%description
${config.description || config.name}

%install
rm -rf %{buildroot}
mkdir -p %{buildroot}
cp -a ${path.join(distDir, 'AppDir')}/. %{buildroot}/

%files
${filesList}

%clean
rm -rf %{buildroot}
`;
                    const specPath = path.join(rpmBuildDir, 'SPECS', `${binName}.spec`);
                    fs.writeFileSync(specPath, specContent);

                    try {
                        execSync(`rpmbuild -bb --define "_topdir ${rpmBuildDir}" "${specPath}"`, { stdio: 'inherit' });
                        const rpmFile = fs.readdirSync(path.join(rpmBuildDir, 'RPMS', 'x86_64'))[0];
                        if (rpmFile) {
                            fs.copyFileSync(
                                path.join(rpmBuildDir, 'RPMS', 'x86_64', rpmFile),
                                path.join(installersDir, rpmFile)
                            );
                            console.log(`Successfully created packages in ${installersDir}`);
                        }
                    } catch (e) {
                        console.error('Failed to build RPM. Is rpmbuild installed?');
                        process.exit(1);
                    }

                    return; // exit the block instead of hitting crabnebula
                } else if (target === 'flatpak') {
                    console.log('Building manual Flatpak via flatpak-builder...');
                    const installersDir = path.join(distDir, 'installers');
                    fs.mkdirSync(installersDir, { recursive: true });

                    const appDirName = await setupAppDir('flatpak');

                    const flatpakBuildDir = path.join(distDir, 'flatpakbuild');
                    const flatpakRepoDir = path.join(distDir, 'flatpakrepo');
                    fs.mkdirSync(flatpakBuildDir, { recursive: true });

                    const appId = config.appId || `org.lotus.${binName}`;

                    const manifest = {
                        "app-id": appId,
                        "runtime": "org.freedesktop.Platform",
                        "runtime-version": "24.08",
                        "sdk": "org.freedesktop.Sdk",
                        "command": binName,
                        "build-options": {
                            "strip": false,
                            "no-debuginfo": true
                        },
                        "finish-args": [
                            "--share=network",
                            "--share=ipc",
                            "--socket=x11",
                            "--socket=wayland",
                            "--device=dri",
                            "--filesystem=host"
                        ],
                        "modules": [
                            {
                                "name": binName,
                                "buildsystem": "simple",
                                "build-commands": [
                                    "cp -a AppDir/usr/* /app/",
                                    `mv /app/share/applications/${binName}.desktop /app/share/applications/\${FLATPAK_ID}.desktop`,
                                    `mv /app/share/icons/hicolor/512x512/apps/${binName}.png /app/share/icons/hicolor/512x512/apps/\${FLATPAK_ID}.png`
                                ],
                                "sources": [
                                    {
                                        "type": "dir",
                                        "path": appDirName,
                                        "dest": "AppDir"
                                    }
                                ]
                            }
                        ]
                    };

                    const manifestPath = path.join(flatpakBuildDir, 'manifest.json');
                    fs.writeFileSync(manifestPath, JSON.stringify(manifest, null, 2));

                    try {
                        execSync(`flatpak-builder --repo=${flatpakRepoDir} --force-clean ${path.join(flatpakBuildDir, 'build')} ${manifestPath}`, { stdio: 'inherit' });
                        const flatpakFile = path.join(installersDir, `${binName}.flatpak`);
                        execSync(`flatpak build-bundle ${flatpakRepoDir} ${flatpakFile} ${appId}`, { stdio: 'inherit' });
                        console.log(`Successfully created packages in ${installersDir}`);
                    } catch (e) {
                        console.error('Failed to build Flatpak. Is flatpak-builder installed?');
                        process.exit(1);
                    }

                    return; // exit the block instead of hitting crabnebula
                }

                // We pass the stringified JSON configuration directly to avoid file resolving bugs in CrabNebula CLI
                const safeConfigJson = JSON.stringify(packagerConfig).replace(/'/g, "'\\''");
                execSync(`npx @crabnebula/packager -c '${safeConfigJson}' ${formatArg}`, { stdio: 'inherit', cwd: distDir });

                console.log(`Successfully created packages in ${path.join(distDir, 'installers')}`);
            }
        } catch (err) {
            console.error('CrabNebula Packaging failed', err);
            process.exit(1);
        }
    });

program
    .command('init [projectName]')
    .description('Initialize a new Lotus project')
    .option('--name <name>', 'Application name')
    .option('--app-version <version>', 'Application version')
    .option('--description <description>', 'Application description')
    .option('--author <author>', 'Author name')
    .option('--license <license>', 'License (default: MIT)')
    .option('--homepage <homepage>', 'Project homepage/repository URL')
    .action(async (projectName, options) => {
        let targetDir = projectName;

        // 1. Prompt for project directory if not provided
        if (!targetDir) {
            const response = await prompts({
                type: 'text',
                name: 'value',
                message: 'Project name (directory):',
                initial: 'my-lotus-app'
            });
            targetDir = response.value;
        }

        if (!targetDir) {
            console.log('Operation cancelled.');
            process.exit(0);
        }

        const projectPath = path.resolve(targetDir);
        if (fs.existsSync(projectPath)) {
            const { overwrite } = await prompts({
                type: 'confirm',
                name: 'overwrite',
                message: `Directory ${targetDir} already exists.Overwrite ? `,
                initial: false
            });
            if (!overwrite) {
                console.log('Aborting.');
                process.exit(1);
            }
        }

        // 2. Gather Metadata (prompts if flags missing)
        const questions = [
            {
                type: options.name ? null : 'text',
                name: 'name',
                message: 'Application Name:',
                initial: targetDir
            },
            {
                type: options.appVersion ? null : 'text',
                name: 'version',
                message: 'Version:',
                initial: '0.1.0'
            },
            {
                type: options.description ? null : 'text',
                name: 'description',
                message: 'Description:',
                initial: 'A Lotus desktop application'
            },
            {
                type: options.author ? null : 'text',
                name: 'author',
                message: 'Author:',
                initial: ''
            },
            {
                type: options.homepage ? null : 'text',
                name: 'homepage',
                message: 'Homepage / Repository URL:',
                initial: ''
            },
            {
                type: options.license ? null : 'text',
                name: 'license',
                message: 'License:',
                initial: 'MIT'
            }
        ];

        const answers = await prompts(questions);
        const metadata = { ...options, ...answers };
        metadata.version = options.appVersion || answers.version;
        // Clean up undefined/null
        Object.keys(metadata).forEach(k => metadata[k] == null && delete metadata[k]);


        // 3. Generate Files
        console.log(`\nCreating project in ${projectPath}...`);
        fs.mkdirSync(projectPath, { recursive: true });

        // package.json
        const packageJson = {
            name: targetDir.toLowerCase().replace(/[^a-z0-9-]/g, '-'), // npm safe name
            version: metadata.version,
            description: metadata.description,
            main: "main.js",
            scripts: {
                "start": "lotus dev main.js",
                "build": "lotus build"
            },
            keywords: ["lotus", "desktop", "gui"],
            author: metadata.author,
            homepage: metadata.homepage,
            license: metadata.license,
            dependencies: {
                "@lotus-gui/core": "latest"
            },
            devDependencies: {
                "@lotus-gui/dev": "latest"
            }
        };
        fs.writeFileSync(path.join(projectPath, 'package.json'), JSON.stringify(packageJson, null, 2));

        // lotus.config.json
        const lotusConfig = {
            name: metadata.name, // Display name
            version: metadata.version,
            license: metadata.license,
            description: metadata.description,
            main: "main.js",
            executableName: targetDir.toLowerCase().replace(/[^a-z0-9-]/g, '-'),
            author: metadata.author,
            homepage: metadata.homepage || undefined,
            build: {
                linux: {
                    wmClass: metadata.name.toLowerCase().replace(/ /g, '-'),
                    categories: ["Utility"]
                }
            }
        };
        // Remove undefined keys
        Object.keys(lotusConfig).forEach(key => lotusConfig[key] === undefined && delete lotusConfig[key]);

        fs.writeFileSync(path.join(projectPath, 'lotus.config.json'), JSON.stringify(lotusConfig, null, 4));

        // main.js
        const mainJs = `const { ServoWindow, app, ipcMain } = require('@lotus-gui/core');
                            const path = require('path');

                            app.warmup();

                            const win = new ServoWindow({
                                id: 'main-window',
                                root: path.join(__dirname, 'ui'),
                                index: 'index.html',
                                width: 1024,
                                height: 768,
                                title: "${metadata.name}",
                                transparent: true,
                                visible: false
                            });

                            win.once('frame-ready', () => win.show());

                            ipcMain.on('hello', (data) => {
                                console.log('Received from renderer:', data);
                                ipcMain.send('reply', { message: 'Hello from Node.js!' });
                            });
                            `;
        fs.writeFileSync(path.join(projectPath, 'main.js'), mainJs);

        // UI Directory
        const uiDir = path.join(projectPath, 'ui');
        fs.mkdirSync(uiDir);

        // ui/index.html
        const indexHtml = `< !DOCTYPE html >
                                <html>
                                    <head>
                                        <title>${metadata.name}</title>
                                        <style>
                                            body {margin: 0; padding: 0; background: transparent; font-family: sans-serif; }
                                            .app {
                                                background: rgba(30, 30, 30, 0.95);
                                            color: white;
                                            height: 100vh;
                                            display: flex;
                                            flex-direction: column;
                                            align-items: center;
                                            justify-content: center;
                                            border-radius: 8px; /* Optional rounded corners for the view */
        }
                                            button {
                                                padding: 10px 20px;
                                            font-size: 16px;
                                            cursor: pointer;
                                            background: #646cff;
                                            color: white;
                                            border: none;
                                            border-radius: 4px;
        }
                                            button:hover {background: #535bf2; }
                                        </style>
                                    </head>
                                    <body>
                                        <div class="app">
                                            <h1>Welcome to ${metadata.name} 🪷</h1>
                                            <p>Powered by Lotus (Servo + Node.js)</p>
                                            <button onclick="sendMessage()">Ping Node.js</button>
                                            <p id="response"></p>
                                        </div>

                                        <script>
                                            function sendMessage() {
                                                window.lotus.send('hello', { timestamp: Date.now() });
        }

        window.lotus.on('reply', (data) => {
                                                document.getElementById('response').innerText = data.message;
        });
                                        </script>
                                    </body>
                                </html>`;
        fs.writeFileSync(path.join(uiDir, 'index.html'), indexHtml);

        console.log(`\n✅ Project initialized in ${projectPath} `);
        console.log(`\nNext steps: `);
        console.log(`  cd ${targetDir} `);
        console.log(`  npm install`);
        console.log(`  npx lotus dev\n`);
    });

program.parse();
